//! A RESP connection: parse pipelined commands, run them against the database's shard under one
//! lock acquisition per batch, write replies, and deliver pushes (pub/sub, unblocked replies).

use crate::database::Database;
use crate::directory::{AuthRequest, Directory, Session};
use crate::repl::psync;
use bytes::{Buf, BufMut, Bytes, BytesMut};
use redis_engine::client::{ConnStats, ReplyBuf, ReplyMode};
use redis_engine::commands::eq;
use redis_engine::commands::replication::ServerAction;
use redis_engine::{Client, Outcome, Reply, Sink};
use redis_proto::{Limits, parse_request};
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(1);

pub fn next_client_id() -> u64 {
    NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed)
}

pub enum Msg {
    Push(Bytes),
    Unblock(Bytes),
    /// The client's ACL user changed: disconnect if it was deleted or lost channel access.
    AclChanged,
    /// CLIENT KILL from another connection.
    Kill,
}

/// Delivers out-of-band data to the connection task.
pub struct ConnSink {
    tx: mpsc::UnboundedSender<Msg>,
}

impl ConnSink {
    pub fn new(tx: mpsc::UnboundedSender<Msg>) -> Self {
        Self { tx }
    }
}

impl Sink for ConnSink {
    fn push(&self, msg: Bytes) {
        let _ = self.tx.send(Msg::Push(msg));
    }
    fn unblock(&self, reply: Bytes) {
        let _ = self.tx.send(Msg::Unblock(reply));
    }
    fn acl_changed(&self) {
        let _ = self.tx.send(Msg::AclChanged);
    }
    fn kill(&self) {
        let _ = self.tx.send(Msg::Kill);
    }
}

/// Most commands executed per lock acquisition, so one pipelining client can't starve others.
const MAX_BATCH: usize = 512;
/// Most bytes taken off the socket per read (Redis reads `PROTO_IOBUF_LEN` at a time).
const READ_CHUNK: usize = 64 * 1024;

pub struct Conn<'a> {
    dir: &'a dyn Directory,
    sni: Option<String>,
    session: Option<Session>,
    client: Client,
    pending: VecDeque<Vec<Bytes>>,
    out: BytesMut,
    skip_next: bool,
    close_after: bool,
    /// Blocked until this Unix ms (0 = forever) or an unblock message.
    blocked: Option<i64>,
    /// More input may be parseable without reading (batch limit reached).
    more: bool,
    stats: Arc<ConnStats>,
    /// REPLCONF options, and the replication start once SYNC/PSYNC accepted the connection.
    repl_conf: psync::ReplicaConf,
    sync: Option<psync::Start>,
    /// `out` measured like Redis' output buffer, when the soft limit was first reached (Unix
    /// seconds), and whether the output buffer limit closed the connection.
    rbuf: ReplyBuf,
    soft_since: i64,
    obuf_closed: bool,
    /// Protocol bytes of the parsed commands in `pending`.
    pending_bytes: u64,
    /// What Redis has parsed off the front of the input still buffered (see `publish_qbuf`).
    qb_skip: usize,
    /// The rest of a big argument being read.
    qb_want: usize,
}

pub async fn serve<S>(stream: S, peer: SocketAddr, local: SocketAddr, dir: Arc<dyn Directory>, fd: i64)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    serve_with(stream, peer, local, dir, None, fd).await
}

/// Serve a connection; `sni` is the TLS server name the client asked for (database routing) and
/// `fd` the socket's descriptor (-1 if none), shown by CLIENT LIST.
pub async fn serve_with<S>(stream: S, peer: SocketAddr, local: SocketAddr, dir: Arc<dyn Directory>, sni: Option<String>, fd: i64)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut rd, mut wr) = tokio::io::split(stream);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let sink: Arc<dyn Sink> = Arc::new(ConnSink { tx });
    let mut client = Client::with_sink(next_client_id(), sink);
    client.addr = peer.to_string();
    client.laddr = local.to_string();
    let stats = Arc::new(ConnStats::default());
    stats.fd.store(fd, Ordering::Relaxed);
    stats.last_interaction.store(redis_engine::util::now_ms() / 1000, Ordering::Relaxed);
    client.conn = Some(stats.clone());
    client.defer_pushes = true;
    let session = dir.anonymous().await;
    if let Some(s) = &session
        && !admit(&s.db)
    {
        // Turned away before reading anything, like Redis' acceptCommonHandler.
        let _ = wr.write_all(b"-ERR max number of clients reached\r\n").await;
        let _ = wr.flush().await;
        return;
    }
    client.authenticated = session.is_some();
    if let Some(s) = &session {
        client.user = s.user.clone();
        s.user.watch(client.id, client.sink.as_ref().unwrap());
        s.db.io.accepted.fetch_add(1, Ordering::Relaxed);
        register(&s.db, &client);
    }
    let mut conn = Conn {
        dir: dir.as_ref(),
        sni,
        session,
        client,
        pending: VecDeque::new(),
        out: BytesMut::with_capacity(16 * 1024),
        skip_next: false,
        close_after: false,
        blocked: None,
        more: false,
        stats,
        repl_conf: Default::default(),
        sync: None,
        rbuf: ReplyBuf::default(),
        soft_since: 0,
        obuf_closed: false,
        pending_bytes: 0,
        qb_skip: 0,
        qb_want: 0,
    };
    let mut inbuf = BytesMut::with_capacity(16 * 1024);
    'conn: loop {
        if conn.blocked.is_none() {
            let max_bulk = conn.db().map_or(Limits::default().max_bulk, |db| db.hub.limits().max_bulk as usize);
            let limits = Limits { authenticated: conn.client.authenticated, max_bulk };
            let queued = conn.pending.len();
            loop {
                match parse_request(&mut inbuf, limits) {
                    Ok(Some(args)) if args.is_empty() => {}
                    Ok(Some(args)) => {
                        conn.pending_bytes += redis_proto::command_len(&args) as u64;
                        conn.pending.push_back(args);
                        if conn.pending.len() >= MAX_BATCH {
                            conn.more = true;
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        Reply::Error(format!("ERR {e}")).encode(conn.client.resp, &mut conn.out);
                        let _ = wr.write_all(&conn.out).await;
                        let _ = wr.shutdown().await;
                        // Take what the client is still sending for a moment, so closing with
                        // unread input doesn't reset the connection before it reads the error.
                        let drain = async {
                            let mut sink = vec![0u8; 64 * 1024];
                            let mut left = 64usize << 20;
                            while left > 0 {
                                match rd.read(&mut sink).await {
                                    Ok(0) | Err(_) => break,
                                    Ok(n) => left = left.saturating_sub(n),
                                }
                            }
                        };
                        let _ = tokio::time::timeout(Duration::from_secs(1), drain).await;
                        break 'conn;
                    }
                }
            }
            let parsed = conn.pending.len().saturating_sub(queued) as u64;
            if parsed > 0 {
                conn.stats.pipeline_sum.fetch_add(parsed, Ordering::Relaxed);
                conn.stats.pipeline_cnt.fetch_add(1, Ordering::Relaxed);
                if let Some(db) = conn.db() {
                    db.io.events.fetch_add(1, Ordering::Relaxed);
                    db.io.pipeline_sum.fetch_add(parsed, Ordering::Relaxed);
                }
            }
            conn.publish_qbuf(&inbuf, false);
            if !conn.pending.is_empty() {
                conn.run_pending().await;
            }
        }
        if !conn.out.is_empty() && !conn.flush(&mut wr, &mut rx).await {
            break;
        }
        // SYNC/PSYNC turned the connection into a replica link.
        if let Some(start) = conn.sync.take() {
            conn.stream_to_replica(&mut rd, &mut wr, &mut inbuf, &mut rx, start).await;
            break;
        }
        if conn.close_after || conn.stats.killed.load(Ordering::Relaxed) {
            break;
        }
        // The batch limit was hit with more commands buffered: keep going without waiting.
        if conn.blocked.is_none() && (!conn.pending.is_empty() || conn.more) {
            conn.more = false;
            continue;
        }
        let deadline = match conn.blocked {
            Some(d) if d > 0 => Some(d),
            _ => None,
        };
        let db = conn.db().cloned();
        // Read a little at a time like Redis (more only for the rest of a big argument), so the
        // query buffer limit sees what a read adds to the unparsed input.
        let mut room = (&mut inbuf).limit(conn.qb_want.max(READ_CHUNK));
        let kicked = async {
            match &db {
                Some(db) => {
                    // Register before checking the flag so a kick in between isn't missed.
                    let n = db.kicked.notified();
                    if !db.evicting.load(Ordering::SeqCst) {
                        n.await;
                    }
                }
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = kicked => break,
            r = rd.read_buf(&mut room) => match r {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    conn.stats.net_in.fetch_add(n as u64, Ordering::Relaxed);
                    conn.stats.read_events.fetch_add(1, Ordering::Relaxed);
                    conn.stats.last_interaction.store(redis_engine::util::now_ms() / 1000, Ordering::Relaxed);
                    if let Some(db) = &db {
                        db.io.reads.fetch_add(1, Ordering::Relaxed);
                        db.net_in.fetch_add(n as u64, Ordering::Relaxed);
                    }
                    if conn.query_buffer_exceeded(inbuf.len().saturating_sub(conn.qb_skip)) {
                        break;
                    }
                    conn.publish_qbuf(&inbuf, true);
                    if conn.evict_clients() {
                        break;
                    }
                }
            },
            Some(msg) = rx.recv() => {
                let mut alive = true;
                let mut next = Some(msg);
                while let Some(msg) = next {
                    alive &= conn.on_msg(msg);
                    next = rx.try_recv().ok();
                }
                if !alive {
                    break;
                }
            }
            _ = sleep_until_ms(deadline.unwrap_or(0)), if deadline.is_some() => {
                conn.on_block_timeout();
            }
        }
    }
    conn.cleanup();
}

/// Attach a connection to `db` unless it already has `maxclients` of them.
fn admit(db: &Database) -> bool {
    let max = db.engine_unless_busy(0).map(|e| e.cfg.maxclients);
    if max.is_some_and(|m| db.io.clients.load(Ordering::Relaxed) >= m) {
        db.io.rejected.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    db.io.clients.fetch_add(1, Ordering::Relaxed);
    true
}

/// Show the connection in CLIENT LIST right away unless the engine is busy (a running script):
/// then it registers with its first command.
fn register(db: &Database, client: &Client) {
    if let Some(mut eng) = db.shards[0].engine.try_lock() {
        eng.register_client(client);
    }
}

async fn sleep_until_ms(at_ms: i64) {
    let now = redis_engine::util::now_ms();
    if at_ms > now {
        tokio::time::sleep(std::time::Duration::from_millis((at_ms - now) as u64)).await;
    }
}

impl Conn<'_> {
    fn db(&self) -> Option<&Arc<Database>> {
        self.session.as_ref().map(|s| &s.db)
    }

    /// The query buffer as Redis would hold it: the header lines and arguments of a command
    /// still being read are parsed out of it (into argv); waiting on a big argument, only that
    /// argument in a buffer of its size.
    /// Input left over once parsed keeps the buffer the client read into as its own; a read
    /// sizes it (at least PROTO_IOBUF_LEN), the cron may shrink it in between.
    fn publish_qbuf(&mut self, inbuf: &BytesMut, read: bool) {
        let p = redis_proto::partial(inbuf);
        let len = inbuf.len() - p.skip;
        self.qb_skip = p.skip;
        self.qb_want = p.big.map_or(0, |n| (n + 2).saturating_sub(len));
        if let Some(n) = p.big {
            self.stats.qbuf_cap.store(n as u64 + 2, Ordering::Relaxed);
        } else if read {
            self.stats.qbuf_cap.store(inbuf.capacity().max(16 * 1024) as u64, Ordering::Relaxed);
        }
        // Decided once the input is parsed (a read is published before that).
        if len > 0 && !read {
            self.stats.qbuf_owned.store(true, Ordering::Relaxed);
        }
        self.stats.qbuf.store(len as u64, Ordering::Relaxed);
        self.stats.partial_argv.store(p.argv as u64, Ordering::Relaxed);
    }

    /// Take the next command to run; until the next one, CLIENT LIST counts it in the query
    /// buffer (as Redis does while running it from there).
    fn pop_pending(&mut self) -> Vec<Bytes> {
        self.stats.pending_input.store(self.pending_bytes, Ordering::Relaxed);
        let args = self.pending.pop_front().unwrap();
        self.pending_bytes -= redis_proto::command_len(&args) as u64;
        args
    }

    /// `maxmemory-clients`: close the largest clients while all of them use too much (this one
    /// included). True if this connection was evicted.
    fn evict_clients(&self) -> bool {
        let Some(db) = self.db() else { return false };
        if db.hub.limits().client_eviction == 0 {
            return false;
        }
        if let Some(mut eng) = db.engine_unless_busy(0) {
            eng.evict_clients(false);
        }
        self.stats.killed.load(Ordering::Relaxed)
    }

    fn write_reply(&mut self, r: &Reply) {
        if self.obuf_closed {
            return;
        }
        let before = self.out.len();
        r.encode(self.client.resp, &mut self.out);
        self.appended(self.out.len() - before);
    }

    fn write_reply_bytes(&mut self, b: &[u8]) {
        if self.client.reply_mode == ReplyMode::Off {
            return;
        }
        self.put(b);
    }

    /// Queue raw output (pushes).
    fn put(&mut self, b: &[u8]) {
        if self.obuf_closed {
            return;
        }
        self.out.extend_from_slice(b);
        self.appended(b.len());
    }

    /// Out-of-band data for the connection. False: close it.
    fn on_msg(&mut self, msg: Msg) -> bool {
        match msg {
            Msg::Push(b) => self.put(&b),
            Msg::Unblock(b) => {
                self.write_reply_bytes(&b);
                self.blocked = None;
            }
            Msg::AclChanged => return !redis_engine::acl::must_disconnect(&self.client),
            Msg::Kill => return false,
        }
        !self.obuf_closed
    }

    fn appended(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        self.rbuf.append(n, &self.stats);
        self.check_obuf();
    }

    /// The `client-output-buffer-limit` class of the connection: replica, pubsub or normal.
    fn obuf_class(&self) -> usize {
        use redis_engine::client::{OBUF_NORMAL, OBUF_PUBSUB, OBUF_REPLICA};
        if self.stats.replica.load(Ordering::Relaxed) {
            OBUF_REPLICA
        } else if self.client.subscriptions() > 0 {
            OBUF_PUBSUB
        } else {
            OBUF_NORMAL
        }
    }

    /// `closeClientOnOutputBufferLimitReached`: past the hard limit, or the soft one for long
    /// enough, the connection is closed without sending what it has pending.
    fn check_obuf(&mut self) {
        self.rbuf.publish(&self.stats);
        if self.rbuf.mem() == 0 || self.obuf_closed {
            return;
        }
        let db = self.db().cloned();
        let limit = db.as_ref().map(|db| db.hub.limits().obuf[self.obuf_class()]).unwrap_or_default();
        let now = redis_engine::util::now_ms() / 1000;
        if !redis_engine::client::obuf_limit_reached(self.rbuf.mem(), &limit, !self.client.authenticated, &mut self.soft_since, now) {
            return;
        }
        tracing::warn!(client = self.client.id, omem = self.rbuf.mem(), "client closed for overcoming of output buffer limits");
        if let Some(db) = &db {
            db.hub.obuf_disconnections.fetch_add(1, Ordering::Relaxed);
        }
        self.obuf_closed = true;
        self.close_after = true;
        self.pending.clear();
        self.pending_bytes = 0;
        self.out.clear();
        self.rbuf.clear();
        self.rbuf.publish(&self.stats);
    }

    /// `client-query-buffer-limit` (1MB before authenticating), counting commands queued in
    /// MULTI: past it the connection is closed.
    fn query_buffer_exceeded(&self, qbuf: usize) -> bool {
        let queued: usize = self.client.multi.as_ref().map_or(0, |m| m.queued.iter().flatten().map(|a| a.len()).sum());
        let used = (qbuf + queued) as u64;
        let limit = self.db().map_or(redis_engine::hub::ConnLimits::default().query_buffer, |db| db.hub.limits().query_buffer);
        if used <= limit && (used <= 1024 * 1024 || self.client.authenticated) {
            return false;
        }
        tracing::warn!(client = self.client.id, qbuf, "closing client that reached max query buffer length");
        if let Some(db) = self.db() {
            db.hub.qbuf_disconnections.fetch_add(1, Ordering::Relaxed);
        }
        true
    }

    /// What the output buffer limit leaves for the next reply before it closes the connection:
    /// a reply past this is sure to break the hard limit.
    fn reply_budget(&self, db: &Database) -> u64 {
        let hard = db.hub.limits().obuf[self.obuf_class()].hard;
        if hard == 0 {
            return u64::MAX;
        }
        hard.saturating_sub(self.rbuf.mem()) + self.rbuf.free() as u64
    }

    /// Write `out` to the socket. While the socket is slow, keep taking pushes (they count
    /// against the output buffer limit, checked again periodically for the soft limit). False:
    /// the connection is gone or must close.
    async fn flush<W: AsyncWrite + Unpin>(&mut self, wr: &mut W, rx: &mut mpsc::UnboundedReceiver<Msg>) -> bool {
        enum Ev {
            Wrote(std::io::Result<usize>),
            Msg(Option<Msg>),
            Tick,
        }
        let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + Duration::from_millis(100), Duration::from_millis(100));
        let mut total = 0u64;
        while !self.out.is_empty() {
            let ev = tokio::select! {
                biased;
                r = wr.write(&self.out) => Ev::Wrote(r),
                m = rx.recv() => Ev::Msg(m),
                _ = tick.tick() => Ev::Tick,
            };
            match ev {
                Ev::Wrote(Ok(0) | Err(_)) => return false,
                Ev::Wrote(Ok(n)) => {
                    self.out.advance(n);
                    self.rbuf.sent(n);
                    self.rbuf.publish(&self.stats);
                    total += n as u64;
                }
                Ev::Msg(Some(m)) => {
                    if !self.on_msg(m) {
                        return false;
                    }
                }
                Ev::Msg(None) => {}
                Ev::Tick => self.check_obuf(),
            }
            if self.obuf_closed {
                return false;
            }
        }
        self.out.clear();
        self.stats.net_out.fetch_add(total, Ordering::Relaxed);
        if let Some(db) = self.db() {
            db.io.writes.fetch_add(1, Ordering::Relaxed);
            db.net_out.fetch_add(total, Ordering::Relaxed);
        }
        true
    }

    /// Apply CLIENT REPLY ON/OFF/SKIP to a finished command's reply.
    fn emit(&mut self, args: &[Bytes], r: Reply) {
        let is_client_reply = args.len() >= 2 && eq(&args[0], "client") && eq(&args[1], "reply");
        if is_client_reply {
            match self.client.reply_mode {
                ReplyMode::On => self.write_reply(&r),
                ReplyMode::Off => {}
                ReplyMode::Skip => {
                    self.client.reply_mode = ReplyMode::On;
                    self.skip_next = true;
                }
            }
            return;
        }
        if self.skip_next {
            self.skip_next = false;
            return;
        }
        if self.client.reply_mode == ReplyMode::Off {
            return;
        }
        self.write_reply(&r);
    }

    async fn run_pending(&mut self) {
        while !self.pending.is_empty() && self.blocked.is_none() && !self.close_after && self.sync.is_none() {
            // A connection whose ACL user was deleted is closed (like Redis does right away).
            if self.client.user.is_deleted() {
                self.pending.clear();
        self.pending_bytes = 0;
                self.close_after = true;
                break;
            }
            // CLIENT PAUSE postpones the command until the pause ends.
            if let Some(db) = self.db().cloned() {
                let args = self.pending.front().unwrap().clone();
                self.wait_unpaused(&db, &args).await;
                if self.stats.killed.load(Ordering::Relaxed) {
                    self.close_after = true;
                    break;
                }
            }
            // Connection-level commands first (they may need to await).
            let args = self.pending.front().unwrap();
            let started = std::time::Instant::now();
            if let Some(r) = self.intercept(args.clone()).await {
                let args = self.pop_pending();
                // HELLO went through the engine already; the rest is accounted here.
                if !eq(&args[0], "hello")
                    && let Some(db) = self.db().cloned()
                {
                    let micros = started.elapsed().as_micros() as u64;
                    db.engine(0).record_external(&self.client, &args, micros, &r);
                }
                self.emit(&args, r);
                continue;
            }
            let Some(db) = self.db().cloned() else {
                let args = self.pop_pending();
                self.emit(&args, Reply::error("NOAUTH Authentication required."));
                continue;
            };
            let mut n = 0u64;
            let before = db.repl_offset();
            let mut actions = Vec::new();
            {
                let Some(mut eng) = db.engine_unless_busy(0) else {
                    let args = self.pop_pending();
                    let r = redis_engine::commands::scripting::while_busy(&db.shard(0).scripts, &args);
                    self.emit(&args, r);
                    continue;
                };
                let mut first = true;
                let evicting = db.hub.limits().client_eviction > 0;
                while let Some(args) = self.pending.front() {
                    if is_intercepted(args, &self.client) || (!first && eng.paused_for(&mut self.client, args).is_some()) {
                        break;
                    }
                    first = false;
                    // Clients past maxmemory-clients go before the command runs.
                    if evicting {
                        self.stats.pending_input.store(self.pending_bytes, Ordering::Relaxed);
                        eng.evict_clients(false);
                        if self.stats.killed.load(Ordering::Relaxed) {
                            self.pending.clear();
                            self.pending_bytes = 0;
                            self.close_after = true;
                            break;
                        }
                    }
                    let args = self.pop_pending();
                    n += 1;
                    self.client.reply_budget = self.reply_budget(&db);
                    let client = &mut self.client;
                    let out = long_running(&args, || eng.call(client, &args));
                    let pushes = eng.take_after_reply();
                    actions.extend(eng.take_server_actions());
                    if actions.iter().any(|a| matches!(a, ServerAction::Restart { .. })) {
                        // Like a crashed or restarting server: no reply, nothing more runs.
                        self.pending.clear();
                        self.pending_bytes = 0;
                        self.close_after = true;
                        break;
                    }
                    match out {
                        Outcome::Reply(r) => {
                            let quit = eq(&args[0], "quit");
                            self.emit(&args, r);
                            for p in pushes {
                                self.put(&p);
                            }
                            if quit || self.obuf_closed || self.client.user.is_deleted() || self.stats.killed.load(Ordering::Relaxed) {
                                self.close_after = true;
                                break;
                            }
                        }
                        Outcome::Blocked => {
                            for p in pushes {
                                self.put(&p);
                            }
                            self.blocked = Some(eng.block_deadline(self.client.id).unwrap_or(0));
                            break;
                        }
                    }
                }
            }
            let restart = actions.iter().find_map(|a| match a {
                ServerAction::Restart { delay_ms } => Some(*delay_ms),
                _ => None,
            });
            if !actions.is_empty() {
                self.server_actions(&db, actions);
            }
            if let Some(delay_ms) = restart {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                self.session = None;
                self.dir.shutdown(db).await;
                return;
            }
            self.stats.pending_input.store(self.pending_bytes, Ordering::Relaxed);
            // Like Redis before sleeping: replies may have grown clients past maxmemory-clients.
            if self.evict_clients() {
                self.pending.clear();
                self.pending_bytes = 0;
                self.close_after = true;
            }
            db.touch(n);
            if db.sync_writes.load(Ordering::Relaxed) {
                let after = db.repl_offset();
                let durability = db.durability.lock().clone();
                if after > before
                    && let Some(d) = durability
                {
                    d.wait(after, None).await;
                }
            }
        }
    }

    /// Wait while CLIENT PAUSE holds `args` back (shown as blocked in CLIENT LIST meanwhile).
    async fn wait_unpaused(&mut self, db: &Arc<Database>, args: &[Bytes]) {
        loop {
            // A busy script holds the engine: the command gets BUSY instead (checked first, as
            // in Redis).
            let end = match db.engine_unless_busy(0) {
                Some(mut eng) => eng.paused_for(&mut self.client, args),
                None => None,
            };
            let Some(end) = end else { break };
            if self.stats.killed.load(Ordering::Relaxed) {
                break;
            }
            self.stats.postponed.store(true, Ordering::Relaxed);
            let left = (end - redis_engine::util::now_ms()).clamp(1, 10);
            tokio::time::sleep(Duration::from_millis(left as u64)).await;
        }
        self.stats.postponed.store(false, Ordering::Relaxed);
    }

    fn on_block_timeout(&mut self) {
        let Some(db) = self.db().cloned() else { return };
        // Through the guard: a re-run (e.g. XREADGROUP CLAIM) can write, and effects must ship.
        let mut eng = db.engine(0);
        let r = eng.block_timeout(self.client.id);
        match r {
            Some(r) => {
                drop(eng);
                self.emit(&[], r);
                self.blocked = None;
            }
            // Served concurrently (the reply is on its way through the sink), or re-run and
            // still blocked until the next deadline.
            None => self.blocked = Some(eng.block_deadline(self.client.id).unwrap_or(0)),
        }
    }

    /// Commands handled here rather than by an engine.
    async fn intercept(&mut self, args: Vec<Bytes>) -> Option<Reply> {
        if !is_intercepted(&args, &self.client) {
            return None;
        }
        let name = &args[0];
        if eq(name, "auth") {
            return Some(self.auth(&args).await);
        }
        if eq(name, "hello") {
            // Authenticate first if requested, then let the engine answer.
            let mut i = 2;
            while i < args.len() {
                if eq(&args[i], "auth") && i + 2 < args.len() {
                    let r = self.auth(&[args[0].clone(), args[i + 1].clone(), args[i + 2].clone()]).await;
                    if r.is_error() {
                        return Some(r);
                    }
                    i += 3;
                } else {
                    i += 1;
                }
            }
            if self.session.is_none() {
                return Some(Reply::error("NOAUTH HELLO must be called with the client already authenticated, otherwise the HELLO <proto> AUTH <user> <pass> option can be used to authenticate the client and select the RESP protocol version at the same time"));
            }
            let db = self.db().cloned().unwrap();
            let r = db.engine(0).call(&mut self.client, &args);
            return Some(match r {
                Outcome::Reply(r) => r,
                Outcome::Blocked => Reply::Nil,
            });
        }
        let db = self.db()?.clone();
        if let Err(r) = redis_engine::acl::check_command(&db.acl, &self.client, &args) {
            return Some(r);
        }
        if eq(name, "monitor") {
            let db = self.db().cloned().unwrap();
            let sink = self.client.sink.clone().unwrap();
            db.hub.add_monitor(self.client.id, redis_engine::hub::Sub { sink, resp: self.client.resp });
            self.client.monitor = true;
            return Some(Reply::Ok);
        }
        if eq(name, "wait") {
            return Some(self.wait(&args).await);
        }
        if eq(name, "waitaof") {
            return Some(self.waitaof(&args).await);
        }
        if eq(name, "shutdown") {
            return Some(self.shutdown(&args).await);
        }
        if eq(name, "replconf") {
            return Some(psync::replconf(&mut self.repl_conf, &args).unwrap_or(Reply::Raw(Bytes::new())));
        }
        if eq(name, "sync") || eq(name, "psync") {
            let (sync, n) = (eq(name, "sync"), args.len());
            if (sync && n != 1) || (!sync && n < 3) {
                return Some(Reply::error(format!("ERR wrong number of arguments for '{}' command", if sync { "sync" } else { "psync" })));
            }
            if !self.out.is_empty() {
                return Some(Reply::error("ERR SYNC and PSYNC are invalid with pending output"));
            }
            return Some(match psync::start(&db, &args, &self.repl_conf) {
                Ok(start) => {
                    self.sync = Some(start);
                    Reply::Raw(Bytes::new())
                }
                Err(r) => r,
            });
        }
        None
    }

    /// Carry out what the engine asked for after a command (REPLICAOF).
    fn server_actions(&mut self, db: &Arc<Database>, actions: Vec<ServerAction>) {
        for action in actions {
            match &action {
                ServerAction::Restart { .. } => continue,
                ServerAction::ChangeReplId => {
                    *db.replid.lock() = crate::database::new_replid();
                    *db.replid2.lock() = None;
                    continue;
                }
                ServerAction::Replicate(args) => {
                    db.publish_raw(&args.iter().map(|a| &a[..]).collect::<Vec<_>>());
                    continue;
                }
                _ => {}
            }
            let upstream = db.upstream.lock().clone();
            if let Some(f) = upstream.as_ref().and_then(|u| u.follower.upgrade()) {
                match action {
                    ServerAction::NoOne => f.promote(),
                    ServerAction::ReplicaOf { .. } => f.stop(),
                    _ => {}
                }
            }
            match action {
                ServerAction::NoOne => {
                    *db.upstream.lock() = None;
                    psync::shift_replid(db);
                }
                ServerAction::ReplicaOf { host, port } => {
                    let (user, pass) = {
                        let eng = db.engine(0);
                        let c = &eng.cfg;
                        (Some(c.masteruser.clone()).filter(|s| !s.is_empty()), Some(c.masterauth.clone()).filter(|s| !s.is_empty()))
                    };
                    let listening_port = self.client.laddr.rsplit(':').next().and_then(|p| p.parse().ok()).unwrap_or(6379);
                    let f = crate::repl::Follower::new(db.clone());
                    *db.upstream.lock() = Some(crate::database::Upstream { host: host.clone(), port, follower: Arc::downgrade(&f), user: true });
                    tokio::spawn(f.follow_redis(psync::MasterConf { host, port, user, pass, listening_port }));
                }
                _ => {}
            }
            // Replicas reconnect to learn the new history.
            db.replicas_reset.notify_waiters();
        }
    }

    /// Stream the database's writes to a replica that SYNC/PSYNC accepted, reading its acks.
    async fn stream_to_replica<R, W>(
        &mut self,
        rd: &mut R,
        wr: &mut W,
        inbuf: &mut BytesMut,
        rx: &mut mpsc::UnboundedReceiver<Msg>,
        start: psync::Start,
    ) where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let Some(db) = self.db().cloned() else { return };
        self.stats.replica.store(true, Ordering::Relaxed);
        if wr.write_all(&start.preamble).await.is_err() {
            return;
        }
        db.io.repl_out.fetch_add(start.preamble.len() as u64, Ordering::Relaxed);
        // `redis-cli --rdb` and other snapshot-only clients: closed after the transfer.
        if start.rdb_only {
            let _ = wr.flush().await;
            return;
        }
        let backlog = start.backlog;
        let ip = self.repl_conf.ip.clone().or_else(|| self.client.addr.rsplit_once(':').map(|(h, _)| h.to_string())).unwrap_or_default();
        let addr = SocketAddr::new(ip.parse().unwrap_or(std::net::Ipv4Addr::LOCALHOST.into()), self.repl_conf.listening_port.clamp(0, 65535) as u16);
        let rid = backlog.register(addr, start.offset);
        db.io.redis_replicas.fetch_add(1, Ordering::Relaxed);
        let mut offset = start.offset;
        let limits = Limits { authenticated: true, ..Limits::default() };
        loop {
            let woken = backlog.wake.notified();
            let reset = db.replicas_reset.notified();
            let Some(chunks) = backlog.read_from(offset) else { break };
            if !chunks.is_empty() {
                let mut ok = true;
                for c in chunks {
                    if wr.write_all(&c).await.is_err() {
                        ok = false;
                        break;
                    }
                    offset += c.len() as u64;
                    db.io.repl_out.fetch_add(c.len() as u64, Ordering::Relaxed);
                }
                if !ok {
                    break;
                }
                continue;
            }
            tokio::select! {
                _ = woken => {}
                _ = reset => break,
                r = rd.read_buf(inbuf) => match r {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        db.io.repl_in.fetch_add(n as u64, Ordering::Relaxed);
                        while let Ok(Some(a)) = parse_request(inbuf, limits) {
                            if let Some(o) = psync::parse_ack(&a) {
                                backlog.ack(rid, o);
                            }
                        }
                    }
                },
                Some(msg) = rx.recv() => if matches!(msg, Msg::Kill) {
                    break;
                },
            }
            if self.stats.killed.load(Ordering::Relaxed) {
                break;
            }
        }
        backlog.unregister(rid);
        db.io.redis_replicas.fetch_sub(1, Ordering::Relaxed);
    }

    /// `WAIT numreplicas timeout`: replicas that acknowledged everything written so far.
    async fn wait(&mut self, args: &[Bytes]) -> Reply {
        if args.len() != 3 {
            return Reply::error("ERR wrong number of arguments for 'wait' command");
        }
        let Some(n) = parse_int(&args[1]) else { return Reply::error("ERR value is not an integer or out of range") };
        let timeout = match parse_timeout(&args[2]) {
            Ok(t) => t,
            Err(e) => return e,
        };
        let db = self.db().cloned().unwrap();
        let offset = db.repl_offset();
        // Redis replicas acknowledge every second; ask them now.
        if n > 0 && db.io.redis_replicas.load(Ordering::Relaxed) > 0 {
            db.publish_raw(&[b"REPLCONF", b"GETACK", b"*"]);
        }
        let backlog = db.backlog.lock().clone();
        match backlog {
            Some(b) => Reply::Int(crate::repl::wait_acks(&b, offset, n.max(0) as usize, timeout).await as i64),
            None if n <= 0 => Reply::Int(0),
            None => {
                // No replicas: wait out the timeout like Redis (0 blocks forever).
                if timeout.is_zero() {
                    std::future::pending::<()>().await;
                }
                tokio::time::sleep(timeout).await;
                Reply::Int(0)
            }
        }
    }

    /// `SHUTDOWN [NOSAVE | SAVE] [NOW] [FORCE] [ABORT]`. The data is always durable, so shutting
    /// down restarts the database: every client is disconnected (this one without a reply) and
    /// the database serves again on the next connection. Unless NOW, replicas first get up to
    /// `shutdown-timeout` seconds to catch up, with writes paused; SHUTDOWN ABORT cancels that.
    async fn shutdown(&mut self, args: &[Bytes]) -> Reply {
        let (mut save, mut nosave, mut now, mut force, mut abort) = (false, false, false, false, false);
        for a in &args[1..] {
            let flag = if eq(a, "nosave") {
                &mut nosave
            } else if eq(a, "save") {
                &mut save
            } else if eq(a, "now") {
                &mut now
            } else if eq(a, "force") {
                &mut force
            } else if eq(a, "abort") {
                &mut abort
            } else {
                return Reply::error("ERR syntax error");
            };
            *flag = true;
        }
        if (abort && (save || nosave || now || force)) || (save && nosave) {
            return Reply::error("ERR syntax error");
        }
        let db = self.db().cloned().unwrap();
        if abort {
            return match db.shutdown.lock().take() {
                Some(n) => {
                    n.notify_one();
                    Reply::Ok
                }
                None => Reply::error("ERR No shutdown in progress."),
            };
        }
        let timeout = db.engine(0).cfg.get(b"shutdown-timeout").first().and_then(|(_, v)| v.parse::<u64>().ok()).unwrap_or(10);
        let replicas = db.io.redis_replicas.load(Ordering::Relaxed) as usize;
        if !now && timeout > 0 && replicas > 0 {
            let cancel = Arc::new(tokio::sync::Notify::new());
            *db.shutdown.lock() = Some(cancel.clone());
            let until = redis_engine::util::now_ms() + timeout as i64 * 1000;
            let paused: Vec<_> = (0..db.shards.len())
                .map(|i| {
                    let mut e = db.engine(i);
                    let prev = e.clients.pause;
                    e.clients.pause = Some((until, false));
                    prev
                })
                .collect();
            let offset = db.repl_offset();
            db.publish_raw(&[b"REPLCONF", b"GETACK", b"*"]);
            let backlog = db.backlog.lock().clone();
            let caught_up = async {
                if let Some(b) = backlog {
                    crate::repl::wait_acks(&b, offset, replicas, std::time::Duration::from_secs(timeout)).await;
                }
            };
            let aborted = tokio::select! {
                _ = caught_up => false,
                _ = cancel.notified() => true,
            };
            db.shutdown.lock().take();
            if aborted {
                for (i, prev) in paused.into_iter().enumerate() {
                    db.engine(i).clients.pause = prev;
                }
                return Reply::error("ERR Errors trying to SHUTDOWN. Check logs.");
            }
        }
        // Release this connection's hold on the database, then restart it.
        self.session = None;
        self.dir.shutdown(db).await;
        self.close_after = true;
        Reply::Raw(Bytes::new())
    }

    /// `WAITAOF numlocal numreplicas timeout`. "Local" durability is the object-storage log.
    async fn waitaof(&mut self, args: &[Bytes]) -> Reply {
        if args.len() != 4 {
            return Reply::error("ERR wrong number of arguments for 'waitaof' command");
        }
        // numlocal 0 or 1, numreplicas not negative, each checked in turn.
        let Some(local) = parse_int(&args[1]) else { return Reply::error("ERR value is not an integer or out of range") };
        if !(0..=1).contains(&local) {
            return Reply::error("ERR value is out of range, value must between 0 and 1");
        }
        let Some(replicas) = parse_int(&args[2]) else { return Reply::error("ERR value is not an integer or out of range") };
        if replicas < 0 {
            return Reply::error("ERR value is out of range, must be positive");
        }
        let timeout = match parse_timeout(&args[3]) {
            Ok(t) => t,
            Err(e) => return e,
        };
        let db = self.db().cloned().unwrap();
        let durability = db.durability.lock().clone();
        if local > 0 && durability.is_none() {
            return Reply::error("ERR WAITAOF cannot be used when numlocal is set but appendonly is disabled.");
        }
        let offset = db.repl_offset();
        let limit = (!timeout.is_zero()).then_some(timeout);
        let started = tokio::time::Instant::now();
        let local_ok = match &durability {
            Some(d) if local > 0 => d.wait(offset, limit).await,
            Some(d) => *d.durable.borrow() >= offset,
            None => false,
        };
        let backlog = db.backlog.lock().clone();
        let acked = match backlog {
            Some(b) if replicas > 0 => {
                let left = if timeout.is_zero() { Duration::ZERO } else { timeout.saturating_sub(started.elapsed()).max(Duration::from_millis(1)) };
                crate::repl::wait_acks(&b, offset, replicas as usize, left).await
            }
            Some(b) => b.acked_count(offset),
            None => 0,
        };
        Reply::Array(vec![Reply::Int(local_ok as i64), Reply::Int(acked as i64)])
    }

    async fn auth(&mut self, args: &[Bytes]) -> Reply {
        let (user, pass) = match args.len() {
            2 => (None, &args[1]),
            3 => (Some(&args[1]), &args[2]),
            // Arity -2, checked before the command runs.
            1 => return Reply::Error(format!("ERR wrong number of arguments for '{}' command", String::from_utf8_lossy(&args[0]).to_ascii_lowercase())),
            _ => return Reply::Error("ERR syntax error".into()),
        };
        let req = AuthRequest { user: user.map(|u| &u[..]), password: pass, sni: self.sni.as_deref(), client: Some(&self.client), command: &args[0] };
        match self.dir.authenticate(req).await {
            Ok(s) => {
                if self.session.as_ref().is_none_or(|old| !Arc::ptr_eq(&old.db, &s.db)) {
                    if !admit(&s.db) {
                        self.close_after = true;
                        return Reply::error("ERR max number of clients reached");
                    }
                    s.db.io.accepted.fetch_add(1, Ordering::Relaxed);
                }
                if self.session.as_ref().is_some_and(|old| !Arc::ptr_eq(&old.db, &s.db)) {
                    // Switching databases: leave the old one completely.
                    self.cleanup();
                    self.client.subs.clear();
                    self.client.psubs.clear();
                    self.client.ssubs.clear();
                    self.client.multi = None;
                }
                if let Some(old) = &self.session {
                    old.user.unwatch(self.client.id);
                }
                s.user.watch(self.client.id, self.client.sink.as_ref().unwrap());
                self.client.user = s.user.clone();
                self.client.authenticated = true;
                register(&s.db, &self.client);
                self.session = Some(s);
                Reply::Ok
            }
            Err(e) => Reply::Error(e.to_string()),
        }
    }

    fn cleanup(&mut self) {
        if let Some(s) = self.session.take() {
            s.db.io.clients.fetch_sub(1, Ordering::Relaxed);
            s.user.unwatch(self.client.id);
            s.db.hub.remove_client(self.client.id);
            for shard in &s.db.shards {
                shard.engine.lock().remove_client(self.client.id);
            }
        }
    }
}

fn parse_int(b: &[u8]) -> Option<i64> {
    redis_engine::util::parse_i64(b)
}

/// Milliseconds, like Redis' `getTimeoutFromObjectOrReply` with `UNIT_MILLISECONDS`.
fn parse_timeout(b: &[u8]) -> Result<Duration, Reply> {
    match parse_int(b) {
        None => Err(Reply::error("ERR timeout is not an integer or out of range")),
        Some(t) if t < 0 => Err(Reply::error("ERR timeout is negative")),
        // The deadline (now + timeout) would overflow.
        Some(t) if t > i64::MAX - redis_engine::util::now_ms() => Err(Reply::error("ERR timeout is out of range")),
        Some(t) => Ok(Duration::from_millis(t as u64)),
    }
}

/// Run `f` for `args`, letting the runtime move this worker's other tasks elsewhere first if the
/// command can run for long (a script: until it ends or is killed from another connection, which
/// must get scheduled meanwhile; MIGRATE: waits on the target over the network; DEBUG: SLEEP,
/// RELOAD, POPULATE).
pub(crate) fn long_running<R>(args: &[Bytes], f: impl FnOnce() -> R) -> R {
    let long = [&b"eval"[..], b"evalsha", b"eval_ro", b"evalsha_ro", b"fcall", b"fcall_ro", b"migrate", b"debug"].iter().any(|n| args[0].eq_ignore_ascii_case(n));
    let multi_thread = tokio::runtime::Handle::try_current().is_ok_and(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread);
    if long && multi_thread { tokio::task::block_in_place(f) } else { f() }
}

fn is_intercepted(args: &[Bytes], client: &Client) -> bool {
    let n = &args[0];
    if eq(n, "auth") || eq(n, "hello") || eq(n, "monitor") {
        return true;
    }
    // Inside MULTI these are queued (or refused) by the engine like any command.
    client.multi.is_none() && (eq(n, "replconf") || eq(n, "sync") || eq(n, "psync") || eq(n, "shutdown") || eq(n, "wait") || eq(n, "waitaof"))
}

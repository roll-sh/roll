//! A RESP connection: parse pipelined commands, run them against the database's shard under one
//! lock acquisition per batch, write replies, and deliver pushes (pub/sub, unblocked replies).

use crate::database::Database;
use crate::directory::{AuthRequest, Directory, Session};
use bytes::{Bytes, BytesMut};
use redis_engine::client::ReplyMode;
use redis_engine::commands::eq;
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
}

/// Most commands executed per lock acquisition, so one pipelining client can't starve others.
const MAX_BATCH: usize = 512;

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
}

pub async fn serve<S>(stream: S, peer: SocketAddr, local: SocketAddr, dir: Arc<dyn Directory>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    serve_with(stream, peer, local, dir, None).await
}

/// Serve a connection; `sni` is the TLS server name the client asked for (database routing).
pub async fn serve_with<S>(stream: S, peer: SocketAddr, local: SocketAddr, dir: Arc<dyn Directory>, sni: Option<String>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut rd, mut wr) = tokio::io::split(stream);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let sink: Arc<dyn Sink> = Arc::new(ConnSink { tx });
    let mut client = Client::with_sink(next_client_id(), sink);
    client.addr = peer.to_string();
    client.laddr = local.to_string();
    let session = dir.anonymous().await;
    client.authenticated = session.is_some();
    if let Some(s) = &session {
        client.user = s.user.clone();
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
    };
    let mut inbuf = BytesMut::with_capacity(16 * 1024);
    'conn: loop {
        if conn.blocked.is_none() {
            let limits = Limits { authenticated: conn.client.authenticated, ..Limits::default() };
            loop {
                match parse_request(&mut inbuf, limits) {
                    Ok(Some(args)) if args.is_empty() => {}
                    Ok(Some(args)) => {
                        conn.pending.push_back(args);
                        if conn.pending.len() >= MAX_BATCH {
                            conn.more = true;
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        Reply::Error(e.to_string()).encode(conn.client.resp, &mut conn.out);
                        let _ = wr.write_all(&conn.out).await;
                        break 'conn;
                    }
                }
            }
            if !conn.pending.is_empty() {
                conn.run_pending().await;
            }
        }
        if !conn.out.is_empty() {
            if wr.write_all(&conn.out).await.is_err() {
                break;
            }
            if let Some(db) = conn.db() {
                db.net_out.fetch_add(conn.out.len() as u64, Ordering::Relaxed);
            }
            conn.out.clear();
        }
        if conn.close_after {
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
            r = rd.read_buf(&mut inbuf) => match r {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Some(db) = &db {
                        db.net_in.fetch_add(n as u64, Ordering::Relaxed);
                    }
                }
            },
            Some(msg) = rx.recv() => {
                match msg {
                    Msg::Push(b) => conn.out.extend_from_slice(&b),
                    Msg::Unblock(b) => {
                        conn.write_reply_bytes(&b);
                        conn.blocked = None;
                    }
                }
                while let Ok(msg) = rx.try_recv() {
                    match msg {
                        Msg::Push(b) => conn.out.extend_from_slice(&b),
                        Msg::Unblock(b) => {
                            conn.write_reply_bytes(&b);
                            conn.blocked = None;
                        }
                    }
                }
            }
            _ = sleep_until_ms(deadline.unwrap_or(0)), if deadline.is_some() => {
                conn.on_block_timeout();
            }
        }
    }
    conn.cleanup();
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

    fn write_reply(&mut self, r: &Reply) {
        r.encode(self.client.resp, &mut self.out);
    }

    fn write_reply_bytes(&mut self, b: &[u8]) {
        if self.client.reply_mode == ReplyMode::Off {
            return;
        }
        self.out.extend_from_slice(b);
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
        while !self.pending.is_empty() && self.blocked.is_none() && !self.close_after {
            // Connection-level commands first (they may need to await).
            let args = self.pending.front().unwrap();
            if let Some(r) = self.intercept(args.clone()).await {
                let args = self.pending.pop_front().unwrap();
                self.emit(&args, r);
                continue;
            }
            let Some(db) = self.db().cloned() else {
                let args = self.pending.pop_front().unwrap();
                self.emit(&args, Reply::error("NOAUTH Authentication required."));
                continue;
            };
            let mut n = 0u64;
            let before = db.repl_offset();
            {
                let mut eng = db.engine(0);
                while let Some(args) = self.pending.front() {
                    if is_intercepted(args) {
                        break;
                    }
                    let args = self.pending.pop_front().unwrap();
                    n += 1;
                    match eng.call(&mut self.client, &args) {
                        Outcome::Reply(r) => {
                            let quit = eq(&args[0], "quit");
                            self.emit(&args, r);
                            if quit {
                                self.close_after = true;
                                break;
                            }
                        }
                        Outcome::Blocked => {
                            self.blocked = Some(eng.block_deadline(self.client.id).unwrap_or(0));
                            break;
                        }
                    }
                }
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

    fn on_block_timeout(&mut self) {
        let Some(db) = self.db().cloned() else { return };
        let r = db.engine(0).block_timeout(self.client.id);
        match r {
            Some(r) => {
                self.emit(&[], r);
                self.blocked = None;
            }
            // Served concurrently: the reply is on its way through the sink.
            None => self.blocked = Some(0),
        }
    }

    /// Commands handled here rather than by an engine.
    async fn intercept(&mut self, args: Vec<Bytes>) -> Option<Reply> {
        if !is_intercepted(&args) {
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
        self.session.as_ref()?;
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
        if eq(name, "save") || eq(name, "bgsave") || eq(name, "bgrewriteaof") {
            return Some(Reply::status(if eq(name, "save") { "OK" } else { "Background saving started" }));
        }
        None
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

    /// `WAITAOF numlocal numreplicas timeout`. "Local" durability is the object-storage log.
    async fn waitaof(&mut self, args: &[Bytes]) -> Reply {
        if args.len() != 4 {
            return Reply::error("ERR wrong number of arguments for 'waitaof' command");
        }
        let (Some(local), Some(replicas)) = (parse_int(&args[1]), parse_int(&args[2])) else {
            return Reply::error("ERR value is not an integer or out of range");
        };
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
            _ => return Reply::Error("ERR syntax error".into()),
        };
        match self.dir.authenticate(AuthRequest { user: user.map(|u| &u[..]), password: pass, sni: self.sni.as_deref() }).await {
            Ok(s) => {
                if self.session.as_ref().is_some_and(|old| !Arc::ptr_eq(&old.db, &s.db)) {
                    // Switching databases: leave the old one completely.
                    self.cleanup();
                    self.client.subs.clear();
                    self.client.psubs.clear();
                    self.client.ssubs.clear();
                    self.client.multi = None;
                }
                self.client.user = s.user.clone();
                self.client.authenticated = true;
                self.session = Some(s);
                Reply::Ok
            }
            Err(e) => Reply::Error(e.to_string()),
        }
    }

    fn cleanup(&mut self) {
        if let Some(s) = self.session.take() {
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
        Some(t) => Ok(Duration::from_millis(t as u64)),
    }
}

fn is_intercepted(args: &[Bytes]) -> bool {
    let n = &args[0];
    eq(n, "auth") || eq(n, "hello") || eq(n, "monitor") || eq(n, "wait") || eq(n, "waitaof") || eq(n, "save") || eq(n, "bgsave") || eq(n, "bgrewriteaof")
}

//! Redis' own replication protocol, both ways. Redis replicas, and tools speaking SYNC/PSYNC
//! (`redis-cli --replica`, `--rdb`, migration tools), follow a roll database; a roll database
//! follows a Redis primary with REPLICAOF (migrating into roll without downtime).
//!
//! The write stream is the database's own feed, so offsets, the backlog and partial
//! resynchronization are shared with roll's internal replicas; Redis replicas additionally get
//! the PINGs and `REPLCONF GETACK *` Redis puts in the stream.

use super::{Backlog, Follower};
use crate::database::{Database, Upstream, new_replid};
use crate::persist::SnapshotCodec;
use crate::snapshot::RdbCodec;
use bytes::{Bytes, BytesMut};
use redis_engine::commands::eq;
use redis_engine::rdb::{self, LoadItem, RdbError, RdbWriter};
use redis_proto::Reply;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub use redis_engine::commands::replication::{ReplicaConf, replconf};

/// Backlog kept for replicas attached with Redis' protocol (`repl-backlog-size`).
pub const BACKLOG_SIZE: usize = 1 << 20;

/// Seconds between the PINGs a primary puts in the stream (`repl-ping-replica-period`).
pub const PING_PERIOD_S: u64 = 10;

fn not_int() -> Reply {
    Reply::error("ERR value is not an integer or out of range")
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn parse_i64(b: &[u8]) -> Option<i64> {
    redis_engine::util::parse_i64(b)
}

/// A replica accepted by SYNC/PSYNC: what to send first, and where the stream continues.
pub struct Start {
    pub preamble: Vec<u8>,
    pub offset: u64,
    pub backlog: Arc<Backlog>,
    pub rdb_only: bool,
}

/// The database's backlog, attached on first use like Redis' `createReplicationBacklogIfNeeded`.
pub fn ensure_backlog(db: &Database) -> Arc<Backlog> {
    if let Some(b) = db.backlog.lock().clone() {
        return b;
    }
    Backlog::attach(db, BACKLOG_SIZE)
}

/// SYNC / PSYNC replid offset: a partial resynchronization from the backlog when it holds the
/// replica's offset under our replication id (or the previous one, up to where it is valid),
/// else a full one from a snapshot taken at a stream offset.
pub fn start(db: &Arc<Database>, args: &[Bytes], conf: &ReplicaConf) -> Result<Start, Reply> {
    let psync = eq(&args[0], "psync");
    if psync && args.len() > 3 && eq(&args[3], "failover") {
        return Err(Reply::error("ERR PSYNC FAILOVER can't be sent to a master."));
    }
    let upstream = db.upstream.lock().clone();
    if let Some(u) = upstream
        && u.follower.upgrade().is_some_and(|f| !f.connected.load(Ordering::SeqCst))
    {
        return Err(Reply::error("NOMASTERLINK Can't SYNC while not connected with my master"));
    }
    if conf.filter.is_some() && !conf.eof {
        return Err(Reply::error("ERR Filtered replica requires EOF capability"));
    }
    let backlog = ensure_backlog(db);
    if psync {
        let Some(off) = parse_i64(&args[2]) else { return Err(not_int()) };
        let asked = lossy(&args[1]);
        let mine = db.replid.lock().clone();
        let id2 = db.replid2.lock().clone();
        let same = asked.eq_ignore_ascii_case(&mine) || id2.is_some_and(|(id, upto)| asked.eq_ignore_ascii_case(&id) && off <= upto as i64);
        if same && off >= 1 && backlog.read_from(off as u64 - 1).is_some() {
            db.io.sync_partial_ok.fetch_add(1, Ordering::Relaxed);
            let preamble = if conf.psync2 { format!("+CONTINUE {mine}\r\n") } else { "+CONTINUE\r\n".to_string() };
            return Ok(Start { preamble: preamble.into_bytes(), offset: off as u64 - 1, backlog, rdb_only: conf.rdb_only });
        }
        if !asked.starts_with('?') {
            db.io.sync_partial_err.fetch_add(1, Ordering::Relaxed);
        }
    }
    db.io.sync_full.fetch_add(1, Ordering::Relaxed);
    let (offset, payload) = {
        let Some(eng) = db.engine_unless_busy(0) else {
            return Err(redis_engine::commands::scripting::while_busy(&db.shard(0).scripts, args));
        };
        let offset = db.snapshot_point();
        let mut out = Vec::new();
        let r = match conf.filter {
            None => RdbCodec.save(&eng, &mut out),
            Some(functions) => save_filtered(&eng, functions, &mut out),
        };
        r.map_err(|e| Reply::error(format!("ERR {e}")))?;
        (offset, out)
    };
    let mut preamble = Vec::with_capacity(payload.len() + 64);
    if psync {
        preamble.extend_from_slice(format!("+FULLRESYNC {} {offset}\r\n", db.replid.lock()).as_bytes());
    }
    preamble.extend_from_slice(format!("${}\r\n", payload.len()).as_bytes());
    preamble.extend_from_slice(&payload);
    Ok(Start { preamble, offset, backlog, rdb_only: conf.rdb_only })
}

/// A snapshot without the keyspace: just the function libraries, or nothing.
fn save_filtered(eng: &redis_engine::Engine, functions: bool, out: &mut Vec<u8>) -> anyhow::Result<()> {
    let now = redis_engine::util::now_ms();
    let ctime = (now / 1000).to_string();
    let used = eng.used_memory().to_string();
    let aux = [
        ("redis-ver", redis_engine::commands::server::REDIS_VERSION),
        ("redis-bits", "64"),
        ("ctime", &ctime),
        ("used-mem", &used),
        ("aof-base", "0"),
    ];
    let mut w = RdbWriter::new(out, &aux, now)?;
    if functions {
        for code in eng.scripting.library_codes() {
            w.function(&code)?;
        }
    }
    w.finish()?;
    Ok(())
}

/// `REPLCONF ACK <offset> [FACK <offset>]` from a replica: its acknowledged offset.
pub fn parse_ack(args: &[Bytes]) -> Option<u64> {
    if args.len() >= 3 && eq(&args[0], "replconf") && eq(&args[1], "ack") {
        return parse_i64(&args[2]).and_then(|o| u64::try_from(o).ok());
    }
    None
}

/// Make a replica database a primary (REPLICAOF NO ONE): the old replication id stays valid up
/// to the current offset, so the former primary's other replicas can continue partially.
pub fn shift_replid(db: &Database) {
    let offset = db.repl_offset();
    let mut id = db.replid.lock();
    *db.replid2.lock() = Some((std::mem::replace(&mut *id, new_replid()), offset + 1));
}

// ---- following a Redis primary ----

/// Where REPLICAOF points and how to authenticate (`masteruser`, `masterauth`).
#[derive(Clone, Debug)]
pub struct MasterConf {
    pub host: String,
    pub port: u16,
    pub user: Option<String>,
    pub pass: Option<String>,
    /// Ours, for `REPLCONF listening-port` (the primary lists it in INFO and ROLE).
    pub listening_port: u16,
}

impl Follower {
    /// Follow a Redis primary until [`Follower::promote`] / [`Follower::stop`], reconnecting
    /// every second like Redis' replication cron, with partial resynchronization when the
    /// primary's backlog still has our offset.
    pub async fn follow_redis(self: Arc<Self>, conf: MasterConf) {
        *self.db.upstream.lock() = Some(Upstream { host: conf.host.clone(), port: conf.port, follower: Arc::downgrade(&self), user: true });
        self.set_state("connect");
        loop {
            let stopped = self.stop.notified();
            tokio::select! {
                _ = stopped => return,
                r = self.redis_link(&conf) => {
                    if self.connected.swap(false, Ordering::SeqCst) {
                        self.link_changed.store(redis_engine::util::now_ms() as u64, Ordering::Relaxed);
                    }
                    self.syncing.store(false, Ordering::Relaxed);
                    self.set_state("connect");
                    if let Err(e) = r {
                        tracing::debug!(host = conf.host, port = conf.port, error = %e, "redis primary link lost");
                    }
                }
            }
            let stopped = self.stop.notified();
            tokio::select! {
                _ = stopped => return,
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        }
    }

    async fn redis_link(&self, conf: &MasterConf) -> anyhow::Result<()> {
        self.set_state("connecting");
        let sock = tokio::time::timeout(Duration::from_secs(60), tokio::net::TcpStream::connect((conf.host.as_str(), conf.port))).await??;
        sock.set_nodelay(true)?;
        let (mut rd, mut wr) = sock.into_split();
        let mut buf = BytesMut::with_capacity(64 << 10);
        self.set_state("handshake");
        // PING first: an authentication error is fine at this point.
        send(&mut wr, &[b"PING"]).await?;
        let pong = read_line(&mut rd, &mut buf).await?;
        if pong.starts_with('-') && !(pong.starts_with("-NOAUTH") || pong.starts_with("-NOPERM") || pong.starts_with("-ERR operation not permitted")) {
            anyhow::bail!("error reply to PING from master: '{pong}'");
        }
        if let Some(pass) = &conf.pass {
            match &conf.user {
                Some(u) => send(&mut wr, &[b"AUTH", u.as_bytes(), pass.as_bytes()]).await?,
                None => send(&mut wr, &[b"AUTH", pass.as_bytes()]).await?,
            }
            let r = read_line(&mut rd, &mut buf).await?;
            if r.starts_with('-') {
                anyhow::bail!("unable to AUTH to MASTER: {r}");
            }
        }
        let port = conf.listening_port.to_string();
        send(&mut wr, &[b"REPLCONF", b"listening-port", port.as_bytes()]).await?;
        read_line(&mut rd, &mut buf).await?;
        send(&mut wr, &[b"REPLCONF", b"capa", b"eof", b"capa", b"psync2"]).await?;
        read_line(&mut rd, &mut buf).await?;
        // Continue our own history: after REPLICAOF NO ONE elsewhere, or a reconnect.
        let (replid, offset) = (self.db.replid.lock().clone(), self.applied() + 1);
        let off = offset.to_string();
        send(&mut wr, &[b"PSYNC", replid.as_bytes(), off.as_bytes()]).await?;
        let reply = loop {
            let l = read_line(&mut rd, &mut buf).await?;
            if !l.is_empty() {
                break l;
            }
        };
        if let Some(rest) = reply.strip_prefix("+FULLRESYNC ") {
            let mut it = rest.split(' ');
            let (Some(id), Some(off)) = (it.next(), it.next().and_then(|o| o.trim().parse::<u64>().ok())) else {
                anyhow::bail!("bad FULLRESYNC reply: {reply}");
            };
            self.set_state("sync");
            self.syncing.store(true, Ordering::Relaxed);
            let payload = read_payload(&mut rd, &mut buf).await?;
            self.db.io.repl_in.fetch_add(payload.len() as u64, Ordering::Relaxed);
            load_full(&self.db, &payload)?;
            self.syncing.store(false, Ordering::Relaxed);
            *self.db.replid.lock() = id.to_string();
            *self.db.replid2.lock() = None;
            self.applied.store(off, Ordering::SeqCst);
            self.db.set_offset(off);
            // Like Redis replicas, keep a backlog: our replicas, and a failover, continue from it.
            ensure_backlog(&self.db).reset(off);
        } else if let Some(rest) = reply.strip_prefix("+CONTINUE") {
            let id = rest.trim();
            if !id.is_empty() && !id.eq_ignore_ascii_case(&replid) {
                // The primary's history continues under a new id (it was promoted).
                let offset = self.applied();
                let mut cur = self.db.replid.lock();
                *self.db.replid2.lock() = Some((std::mem::replace(&mut *cur, id.to_string()), offset + 1));
            }
        } else {
            anyhow::bail!("unexpected reply to PSYNC from master: {reply}");
        }
        self.connected.store(true, Ordering::SeqCst);
        self.set_state("connected");
        let now = redis_engine::util::now_ms() as u64;
        self.link_changed.store(now, Ordering::Relaxed);
        self.last_contact.store(now, Ordering::Relaxed);
        let ack = |off: u64| {
            let s = off.to_string();
            let mut out = BytesMut::new();
            redis_proto::encode_command([b"REPLCONF".as_slice(), b"ACK", s.as_bytes()], &mut out);
            out
        };
        wr.write_all(&ack(self.applied())).await?;
        let mut dbsel = 0u16;
        for o in self.apply_redis(&mut buf, &mut dbsel)? {
            wr.write_all(&ack(o)).await?;
        }
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                r = rd.read_buf(&mut buf) => {
                    let n = r?;
                    if n == 0 {
                        anyhow::bail!("master closed the link");
                    }
                    self.db.io.repl_in.fetch_add(n as u64, Ordering::Relaxed);
                    self.last_contact.store(redis_engine::util::now_ms() as u64, Ordering::Relaxed);
                    for o in self.apply_redis(&mut buf, &mut dbsel)? {
                        wr.write_all(&ack(o)).await?;
                    }
                }
                _ = tick.tick() => {
                    wr.write_all(&ack(self.applied())).await?;
                    let idle = redis_engine::util::now_ms() as u64 - self.last_contact.load(Ordering::Relaxed);
                    if idle > 60_000 {
                        anyhow::bail!("MASTER timeout: no data nor PING received");
                    }
                }
            }
        }
    }

    /// Apply the complete commands in `buf`. Every byte counts in the offset, PINGs and
    /// `REPLCONF GETACK` included; returns the offsets GETACKs asked to acknowledge.
    fn apply_redis(&self, buf: &mut BytesMut, dbsel: &mut u16) -> anyhow::Result<Vec<u64>> {
        let mut effects = Vec::new();
        let mut acks = Vec::new();
        let mut offset = self.applied();
        let raw = buf.clone().freeze();
        let limits = redis_proto::Limits { authenticated: true, ..Default::default() };
        loop {
            let before = buf.len();
            let Some(args) = redis_proto::parse_request(buf, limits).map_err(|e| anyhow::anyhow!("corrupt stream: {e}"))? else { break };
            let len = (before - buf.len()) as u64;
            if args.is_empty() {
                offset += len;
                continue;
            }
            if eq(&args[0], "replconf") {
                if args.len() >= 2 && eq(&args[1], "getack") {
                    acks.push(offset);
                }
            } else if args.len() == 2 && eq(&args[0], "select") {
                *dbsel = std::str::from_utf8(&args[1])?.parse()?;
            } else if !eq(&args[0], "ping") {
                effects.push(redis_engine::Effect { db: *dbsel, args });
            }
            offset += len;
        }
        let consumed = (offset - self.applied()) as usize;
        if consumed > 0 {
            let mut eng = self.db.engine(0);
            if !effects.is_empty() {
                eng.apply_all(&effects);
            }
            // Our stream is the primary's, byte for byte (not the effects re-encoded), so our
            // replicas and backlog continue it, also after a failover.
            eng.take_effects();
            self.db.publish_bytes(raw.slice(..consumed));
            self.applied.store(offset, Ordering::SeqCst);
        }
        Ok(acks)
    }
}

async fn send(wr: &mut (impl AsyncWrite + Unpin), args: &[&[u8]]) -> anyhow::Result<()> {
    let mut out = BytesMut::new();
    redis_proto::encode_command(args, &mut out);
    wr.write_all(&out).await?;
    Ok(())
}

/// One reply line without its CRLF (a bare `\n` keepalive reads as an empty line).
async fn read_line(rd: &mut (impl AsyncRead + Unpin), buf: &mut BytesMut) -> anyhow::Result<String> {
    loop {
        if let Some(p) = buf.iter().position(|&c| c == b'\n') {
            let l = buf.split_to(p + 1);
            let l = l.strip_suffix(b"\n").unwrap_or(&l);
            let l = l.strip_suffix(b"\r").unwrap_or(l);
            return Ok(String::from_utf8_lossy(l).into_owned());
        }
        let r = tokio::time::timeout(Duration::from_secs(60), rd.read_buf(buf)).await??;
        if r == 0 {
            anyhow::bail!("master closed the link");
        }
    }
}

/// The snapshot after FULLRESYNC: `$<len>\r\n<bytes>` (disk-based) or `$EOF:<mark>\r\n<bytes><mark>`
/// (diskless), after any `\n` keepalives sent while the primary prepares it.
async fn read_payload(rd: &mut (impl AsyncRead + Unpin), buf: &mut BytesMut) -> anyhow::Result<Vec<u8>> {
    let header = loop {
        let l = read_line(rd, buf).await?;
        if !l.is_empty() {
            break l;
        }
    };
    let Some(spec) = header.strip_prefix('$') else { anyhow::bail!("bad protocol from MASTER, the first byte is not '$': {header}") };
    if let Some(mark) = spec.strip_prefix("EOF:") {
        let mark = mark.as_bytes().to_vec();
        if mark.len() != 40 {
            anyhow::bail!("bad EOF mark from MASTER");
        }
        loop {
            if buf.len() >= mark.len() && buf.ends_with(&mark) {
                let mut data = buf.split().to_vec();
                data.truncate(data.len() - mark.len());
                return Ok(data);
            }
            if tokio::time::timeout(Duration::from_secs(60), rd.read_buf(buf)).await?? == 0 {
                anyhow::bail!("master closed the link during the transfer");
            }
        }
    }
    let len: usize = spec.trim().parse()?;
    while buf.len() < len {
        if tokio::time::timeout(Duration::from_secs(60), rd.read_buf(buf)).await?? == 0 {
            anyhow::bail!("master closed the link during the transfer");
        }
    }
    Ok(buf.split_to(len).to_vec())
}

/// Replace the database with a primary's snapshot (data and function libraries).
fn load_full(db: &Database, data: &[u8]) -> anyhow::Result<()> {
    let now = redis_engine::util::now_ms();
    let mut eng = db.engine(0);
    let cfg = eng.cfg.clone();
    // Parse first: a broken transfer leaves the current data in place.
    let mut entries = Vec::new();
    let mut functions = Vec::new();
    let mut untemplate = Vec::new();
    let reg = eng.hub.templates.clone();
    rdb::load_with(data, &cfg, Some(&reg), |it| {
        match it {
            LoadItem::Function(code) => functions.push(code),
            LoadItem::Entry { db, key, entry } if !entry.expired(now) => entries.push((db, key, entry)),
            LoadItem::Untemplate { db, key } => untemplate.push((db, key)),
            _ => {}
        }
        Ok::<(), RdbError>(())
    })
    .map_err(|e| anyhow::anyhow!("MASTER sent a bad RDB: {e}"))?;
    eng.flush_all();
    eng.scripting
        .restore_libraries(&functions, redis_engine::commands::scripting::RestorePolicy::Flush)
        .map_err(|e| anyhow::anyhow!("MASTER sent bad functions: {e}"))?;
    for (i, key, entry) in entries {
        let Some(d) = eng.dbs.get_mut(i) else { anyhow::bail!("MASTER sent database {i}, beyond `databases`") };
        if !d.keys.contains_key(&key[..]) {
            d.insert_new(&key, entry);
        }
    }
    let lim = redis_engine::types::hash::Limits::of(&cfg);
    for (i, key) in untemplate {
        if let Some(d) = eng.dbs.get_mut(i) {
            d.untemplate(&key, lim);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<Bytes> {
        s.iter().map(|a| Bytes::copy_from_slice(a.as_bytes())).collect()
    }

    #[test]
    fn acks() {
        assert_eq!(parse_ack(&args(&["REPLCONF", "ACK", "42"])), Some(42));
        assert_eq!(parse_ack(&args(&["REPLCONF", "ACK", "42", "FACK", "40"])), Some(42));
        assert_eq!(parse_ack(&args(&["PING"])), None);
    }
}

//! Connection and server commands that a shard engine can answer itself. Commands that need the
//! server layer (AUTH, CLIENT LIST/KILL, replication, cluster…) are declared here too, so COMMAND
//! and ACL know them, but are intercepted by `redis-server` before reaching an engine.

use super::*;
use crate::client::ReplyMode;
use crate::util::now_us;
use crate::{cmd, container};

/// The Redis version we are compatible with (reported by INFO and HELLO).
pub const REDIS_VERSION: &str = "8.10.2";

const CONN_FLAGS: u64 = NOSCRIPT | LOADING | STALE | FAST | NO_AUTH | ALLOW_BUSY;

pub static COMMANDS: &[Command] = &[
    cmd!("ping", -1, FAST | SUBSCRIBED_OK, 0, 0, 0, ping, Connection),
    cmd!("echo", 2, FAST | LOADING | STALE, 0, 0, 0, echo, Connection),
    cmd!("select", 2, LOADING | STALE | FAST, 0, 0, 0, select, Connection),
    cmd!("swapdb", 3, WRITE | FAST, 0, 0, 0, swapdb, Server),
    cmd!("dbsize", 1, READONLY | FAST | ALL_SHARDS, 0, 0, 0, dbsize, Server),
    cmd!("flushdb", -1, WRITE | ALL_SHARDS | DANGEROUS, 0, 0, 0, flushdb, Server),
    cmd!("flushall", -1, WRITE | ALL_SHARDS | DANGEROUS, 0, 0, 0, flushall, Server),
    cmd!("time", 1, LOADING | STALE | FAST, 0, 0, 0, time, Server),
    cmd!("lastsave", 1, LOADING | STALE | FAST, 0, 0, 0, lastsave, Server),
    cmd!("info", -1, LOADING | STALE | DANGEROUS, 0, 0, 0, super::info::info, Server),
    cmd!("lolwut", -1, READONLY | FAST, 0, 0, 0, super::lolwut::lolwut, Server),
    cmd!("hello", -1, CONN_FLAGS, 0, 0, 0, hello, Connection),
    cmd!("reset", 1, CONN_FLAGS | SUBSCRIBED_OK | TXN_CONTROL, 0, 0, 0, reset, Connection),
    cmd!("quit", -1, CONN_FLAGS | SUBSCRIBED_OK | TXN_CONTROL, 0, 0, 0, quit, Connection),
    cmd!("auth", -2, CONN_FLAGS, 0, 0, 0, auth_in_exec, Connection),
    cmd!("monitor", 1, ADMIN | NOSCRIPT | LOADING | STALE, 0, 0, 0, server_only, Server),
    cmd!("wait", 3, BLOCKING, 0, 0, 0, wait_queued, Generic),
    cmd!("waitaof", 4, BLOCKING, 0, 0, 0, waitaof_queued, Generic),
    cmd!("save", 1, ADMIN | NOSCRIPT | NO_ASYNC_LOADING | NO_MULTI, 0, 0, 0, super::persist::save, Server),
    cmd!("bgsave", -1, ADMIN | NOSCRIPT | NO_ASYNC_LOADING, 0, 0, 0, super::persist::bgsave, Server),
    cmd!("bgrewriteaof", 1, ADMIN | NOSCRIPT | NO_ASYNC_LOADING, 0, 0, 0, super::persist::bgrewriteaof, Server),
    cmd!("shutdown", -1, ADMIN | NOSCRIPT | LOADING | STALE | NO_MULTI | ALLOW_BUSY, 0, 0, 0, server_only, Server),
    cmd!("replicaof", 3, ADMIN | NOSCRIPT | STALE | NO_ASYNC_LOADING, 0, 0, 0, super::replication::replicaof, Server),
    cmd!("slaveof", 3, ADMIN | NOSCRIPT | STALE | NO_ASYNC_LOADING, 0, 0, 0, super::replication::replicaof, Server),
    cmd!("role", 1, NOSCRIPT | LOADING | STALE | FAST, 0, 0, 0, role, Server),
    cmd!("sync", 1, ADMIN | NOSCRIPT | NO_ASYNC_LOADING | NO_MULTI, 0, 0, 0, server_only, Server),
    cmd!("psync", -3, ADMIN | NOSCRIPT | NO_ASYNC_LOADING | NO_MULTI, 0, 0, 0, server_only, Server),
    cmd!("replconf", -1, ADMIN | NOSCRIPT | LOADING | STALE | ALLOW_BUSY, 0, 0, 0, super::replication::replconf_cmd, Server),
    cmd!("failover", -1, ADMIN | NOSCRIPT | STALE, 0, 0, 0, server_only, Server),
    cmd!("readonly", 1, LOADING | STALE | FAST, 0, 0, 0, readonly, Cluster),
    cmd!("readwrite", 1, LOADING | STALE | FAST, 0, 0, 0, readwrite, Cluster),
    cmd!("asking", 1, FAST, 0, 0, 0, asking, Cluster),
    container!("cluster", 0, Cluster, CLUSTER),
    container!("client", 0, Connection, super::client::CLIENT),
    container!("config", 0, Server, CONFIG),
    // Bare COMMAND lists every command (arity -1, unlike other containers).
    Command { arity: -1, handler: command_all, ..container!("command", LOADING | STALE, Server, COMMAND) },
    container!("memory", 0, Server, MEMORY),
    container!("slowlog", 0, Server, SLOWLOG),
    container!("latency", 0, Server, LATENCY),
    container!("module", 0, Server, MODULE),
    // Redis declares BACKUP with arity 2 (every subcommand takes no argument).
    Command { arity: 2, ..container!("backup", 0, Server, BACKUP) },
    cmd!("debug", -2, ADMIN | NOSCRIPT | LOADING | STALE | DANGEROUS, 0, 0, 0, super::debug::debug, Server),
];

/// Cluster mode is disabled (as on a standalone Redis): every subcommand answers so.
static CLUSTER: &[Command] = &[
    cmd!("addslots", -3, ADMIN | STALE | NO_ASYNC_LOADING, 0, 0, 0, cluster_disabled, Cluster),
    cmd!("addslotsrange", -4, ADMIN | STALE | NO_ASYNC_LOADING, 0, 0, 0, cluster_disabled, Cluster),
    cmd!("bumpepoch", 2, ADMIN | STALE | NO_ASYNC_LOADING, 0, 0, 0, cluster_disabled, Cluster),
    cmd!("count-failure-reports", 3, ADMIN | LOADING | STALE, 0, 0, 0, cluster_disabled, Cluster),
    cmd!("countkeysinslot", 3, STALE, 0, 0, 0, cluster_disabled, Cluster),
    cmd!("delslots", -3, ADMIN | STALE | NO_ASYNC_LOADING, 0, 0, 0, cluster_disabled, Cluster),
    cmd!("delslotsrange", -4, ADMIN | STALE | NO_ASYNC_LOADING, 0, 0, 0, cluster_disabled, Cluster),
    cmd!("failover", -2, ADMIN | STALE | NO_ASYNC_LOADING, 0, 0, 0, cluster_disabled, Cluster),
    cmd!("flushslots", 2, ADMIN | STALE | NO_ASYNC_LOADING, 0, 0, 0, cluster_disabled, Cluster),
    cmd!("forget", 3, ADMIN | STALE | NO_ASYNC_LOADING, 0, 0, 0, cluster_disabled, Cluster),
    cmd!("getkeysinslot", 4, STALE, 0, 0, 0, cluster_disabled, Cluster),
    cmd!("help", 2, LOADING | STALE, 0, 0, 0, cluster_disabled, Cluster),
    cmd!("info", 2, LOADING | STALE, 0, 0, 0, cluster_disabled, Cluster),
    cmd!("keyslot", 3, LOADING | STALE, 0, 0, 0, cluster_disabled, Cluster),
    cmd!("links", 2, LOADING | STALE, 0, 0, 0, cluster_disabled, Cluster),
    cmd!("meet", -4, ADMIN | STALE | NO_ASYNC_LOADING, 0, 0, 0, cluster_disabled, Cluster),
    cmd!("migration", -4, ADMIN | STALE | NO_ASYNC_LOADING, 0, 0, 0, cluster_disabled, Cluster),
    cmd!("myid", 2, LOADING | STALE, 0, 0, 0, cluster_disabled, Cluster),
    cmd!("myshardid", 2, LOADING | STALE, 0, 0, 0, cluster_disabled, Cluster),
    cmd!("nodes", 2, LOADING | STALE, 0, 0, 0, cluster_disabled, Cluster),
    cmd!("replicas", 3, ADMIN | LOADING | STALE, 0, 0, 0, cluster_disabled, Cluster),
    cmd!("replicate", 3, ADMIN | STALE | NO_ASYNC_LOADING, 0, 0, 0, cluster_disabled, Cluster),
    cmd!("reset", -2, ADMIN | NOSCRIPT | STALE, 0, 0, 0, cluster_disabled, Cluster),
    cmd!("saveconfig", 2, ADMIN | STALE | NO_ASYNC_LOADING, 0, 0, 0, cluster_disabled, Cluster),
    cmd!("set-config-epoch", 3, ADMIN | STALE | NO_ASYNC_LOADING, 0, 0, 0, cluster_disabled, Cluster),
    cmd!("setslot", -4, ADMIN | STALE | NO_ASYNC_LOADING, 0, 0, 0, cluster_disabled, Cluster),
    cmd!("shards", 2, LOADING | STALE, 0, 0, 0, cluster_disabled, Cluster),
    cmd!("slaves", 3, ADMIN | LOADING | STALE, 0, 0, 0, cluster_disabled, Cluster),
    cmd!("slot-stats", -4, LOADING | STALE, 0, 0, 0, cluster_disabled, Cluster),
    cmd!("slots", 2, LOADING | STALE, 0, 0, 0, cluster_disabled, Cluster),
    cmd!("syncslots", -3, ADMIN | STALE | NO_ASYNC_LOADING, 0, 0, 0, cluster_disabled, Cluster),
];

static CONFIG: &[Command] = &[
    cmd!("get", -3, ADMIN | NOSCRIPT | LOADING | STALE, 0, 0, 0, config_get, Server),
    cmd!("set", -4, ADMIN | NOSCRIPT | LOADING | STALE, 0, 0, 0, config_set, Server),
    cmd!("resetstat", 2, ADMIN | NOSCRIPT | LOADING | STALE, 0, 0, 0, config_resetstat, Server),
    cmd!("rewrite", 2, ADMIN | NOSCRIPT | LOADING | STALE, 0, 0, 0, config_rewrite, Server),
    cmd!("help", 2, LOADING | STALE, 0, 0, 0, config_help, Server),
];

static COMMAND: &[Command] = &[
    cmd!("count", 2, LOADING | STALE, 0, 0, 0, command_count, Server),
    cmd!("list", -2, LOADING | STALE, 0, 0, 0, command_list, Server),
    cmd!("info", -2, LOADING | STALE, 0, 0, 0, command_info, Server),
    cmd!("docs", -2, LOADING | STALE, 0, 0, 0, command_docs, Server),
    cmd!("getkeys", -3, LOADING | STALE, 0, 0, 0, command_getkeys, Server),
    cmd!("getkeysandflags", -3, LOADING | STALE, 0, 0, 0, command_getkeysandflags, Server),
    cmd!("help", 2, LOADING | STALE, 0, 0, 0, command_help, Server),
];

static MEMORY: &[Command] = &[
    cmd!("usage", -3, READONLY, 2, 2, 1, memory_usage, Server),
    cmd!("stats", 2, 0, 0, 0, 0, memory_stats, Server),
    cmd!("doctor", 2, 0, 0, 0, 0, memory_doctor, Server),
    cmd!("malloc-stats", 2, 0, 0, 0, 0, memory_malloc_stats, Server),
    cmd!("purge", 2, 0, 0, 0, 0, memory_purge, Server),
    cmd!("help", 2, LOADING | STALE, 0, 0, 0, memory_help, Server),
];

static BACKUP: &[Command] = &[
    cmd!("abort", 2, ADMIN | NOSCRIPT, 0, 0, 0, super::persist::backup_abort, Server),
    cmd!("cleanup", 2, ADMIN | NOSCRIPT, 0, 0, 0, super::persist::backup_cleanup, Server),
    cmd!("help", 2, LOADING | STALE, 0, 0, 0, super::persist::backup_help, Server),
    cmd!("list", 2, ADMIN | STALE, 0, 0, 0, super::persist::backup_list, Server),
    cmd!("seal", 2, ADMIN | NOSCRIPT, 0, 0, 0, super::persist::backup_seal, Server),
    cmd!("start", 2, ADMIN | NOSCRIPT, 0, 0, 0, super::persist::backup_start, Server),
    cmd!("status", 2, ADMIN | STALE, 0, 0, 0, super::persist::backup_status, Server),
];

static SLOWLOG: &[Command] = &[
    cmd!("get", -2, ADMIN | LOADING | STALE, 0, 0, 0, slowlog_get, Server),
    cmd!("len", 2, ADMIN | LOADING | STALE, 0, 0, 0, slowlog_len, Server),
    cmd!("reset", 2, ADMIN | LOADING | STALE, 0, 0, 0, slowlog_reset, Server),
    cmd!("help", 2, LOADING | STALE, 0, 0, 0, slowlog_help, Server),
];

static LATENCY: &[Command] = &[
    cmd!("latest", 2, ADMIN | NOSCRIPT | LOADING | STALE, 0, 0, 0, latency_latest, Server),
    cmd!("history", 3, ADMIN | NOSCRIPT | LOADING | STALE, 0, 0, 0, latency_history, Server),
    cmd!("reset", -2, ADMIN | NOSCRIPT | LOADING | STALE, 0, 0, 0, latency_reset, Server),
    cmd!("doctor", 2, ADMIN | NOSCRIPT | LOADING | STALE, 0, 0, 0, latency_doctor, Server),
    cmd!("graph", 3, ADMIN | NOSCRIPT | LOADING | STALE, 0, 0, 0, latency_graph, Server),
    cmd!("histogram", -2, ADMIN | NOSCRIPT | LOADING | STALE, 0, 0, 0, latency_histogram, Server),
    cmd!("help", 2, LOADING | STALE, 0, 0, 0, latency_help, Server),
];

static MODULE: &[Command] = &[
    cmd!("list", 2, ADMIN | NOSCRIPT, 0, 0, 0, module_list, Server),
    cmd!("load", -3, ADMIN | NOSCRIPT | NO_ASYNC_LOADING, 0, 0, 0, module_denied, Server),
    cmd!("loadex", -3, ADMIN | NOSCRIPT | NO_ASYNC_LOADING, 0, 0, 0, module_denied, Server),
    cmd!("unload", 3, ADMIN | NOSCRIPT | NO_ASYNC_LOADING, 0, 0, 0, module_denied, Server),
    cmd!("help", 2, LOADING | STALE, 0, 0, 0, module_help, Server),
];

pub fn ping(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() > 2 {
        return Err(arity_error("ping"));
    }
    if cx.client.resp == 2 && cx.client.subscriptions() > 0 {
        return Ok(Reply::Array(vec![Reply::bulk("pong"), Reply::Bulk(args.get(1).cloned().unwrap_or_default())]));
    }
    Ok(match args.get(1) {
        Some(m) => Reply::Bulk(m.clone()),
        None => Reply::status("PONG"),
    })
}

pub fn echo(_cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    Ok(Reply::Bulk(args[1].clone()))
}

/// A DB index: an `int` (`getIntFromObjectOrReply`), then in range.
fn db_int(a: &[u8], msg: Option<&'static str>) -> Result<i64, Error> {
    super::lists::long_in(a, i32::MIN as i64, i32::MAX as i64, msg)
}

fn db_range(cx: &Cx, n: i64) -> Result<usize, Error> {
    if n < 0 || n as usize >= cx.eng.dbs.len() {
        return Err(Error::new("ERR DB index is out of range"));
    }
    Ok(n as usize)
}

fn db_arg(cx: &Cx, a: &[u8], msg: Option<&'static str>) -> Result<usize, Error> {
    db_range(cx, db_int(a, msg)?)
}

pub fn select(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.client.db = db_arg(cx, &args[1], None)?;
    Ok(Reply::Ok)
}

pub fn swapdb(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    // Both parsed before either is range-checked.
    let a = db_int(&args[1], Some("ERR invalid first DB index"))?;
    let b = db_int(&args[2], Some("ERR invalid second DB index"))?;
    let (a, b) = (db_range(cx, a)?, db_range(cx, b)?);
    if a != b {
        cx.eng.touch_all_watched(Some(a as u16));
        cx.eng.touch_all_watched(Some(b as u16));
        cx.eng.dbs.swap(a, b);
        cx.eng.touch_all_watched(Some(a as u16));
        cx.eng.touch_all_watched(Some(b as u16));
        // Clients blocked on keys that now exist in the swapped db can be served.
        let mut ready = Vec::new();
        for (db, key) in cx.eng.blocking.by_key.keys() {
            if (*db as usize == a || *db as usize == b) && cx.eng.dbs[*db as usize].keys.contains_key(key) {
                ready.push((*db, key.clone()));
            }
        }
        for k in ready {
            if cx.eng.blocking.ready_set.insert(k.clone()) {
                cx.eng.blocking.ready.push(k);
            }
        }
    }
    cx.eng.sig.dirty += 1;
    Ok(Reply::Ok)
}

/// `getTimeoutFromObjectOrReply` in milliseconds (WAIT, WAITAOF).
fn wait_timeout(a: &[u8], now: i64) -> Result<(), Error> {
    match crate::util::parse_i64(a) {
        None => Err(Error::new("ERR timeout is not an integer or out of range")),
        Some(t) if t < 0 => Err(Error::new("ERR timeout is negative")),
        Some(t) if t > i64::MAX - now => Err(Error::new("ERR timeout is out of range")),
        Some(_) => Ok(()),
    }
}

/// Replicas that acknowledged everything written so far.
fn acked_replicas(cx: &Cx) -> i64 {
    let r = cx.eng.host.as_ref().map(|h| h.replication()).unwrap_or_default();
    r.replicas.iter().filter(|rep| rep.offset >= r.offset).count() as i64
}

/// WAIT inside a transaction (the connection runs it otherwise): it can't block, so it
/// answers with the replicas already caught up.
pub fn wait_queued(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if cx.eng.replica {
        return Err(Error::new(
            "ERR WAIT cannot be used with replica instances. Please also note that since Redis 4.0 if a replica is configured to be writable (which is not the default) writes to replicas are just local and are not propagated.",
        ));
    }
    int(&args[1])?;
    wait_timeout(&args[2], cx.now)?;
    Ok(Reply::Int(acked_replicas(cx)))
}

/// WAITAOF inside a transaction: no blocking either.
pub fn waitaof_queued(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let local = super::lists::long_in(&args[1], 0, 1, None)?;
    if int(&args[2])? < 0 {
        return Err(Error::new("ERR value is out of range, must be positive"));
    }
    wait_timeout(&args[3], cx.now)?;
    if cx.eng.replica {
        return Err(Error::new("ERR WAITAOF cannot be used with replica instances. Please also note that writes to replicas are just local and are not propagated."));
    }
    let aof = cx.eng.host.as_ref().is_some_and(|h| h.aof_enabled());
    if local > 0 && !aof {
        return Err(Error::new("ERR WAITAOF cannot be used when numlocal is set but appendonly is disabled."));
    }
    Ok(Reply::Array(vec![Reply::Int(aof as i64), Reply::Int(acked_replicas(cx))]))
}

pub fn dbsize(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(Reply::Int(cx.db().len() as i64))
}

fn flush_mode(args: &[Bytes]) -> Result<(), Error> {
    match args.len() {
        1 => Ok(()),
        2 if eq(&args[1], "async") || eq(&args[1], "sync") => Ok(()),
        _ => Err(SYNTAX),
    }
}

/// Does this flush free the data in Redis' lazyfree thread (ASYNC, or a SYNC flush run as a
/// blocking async one outside MULTI and scripts)? Hash template references are then
/// released by the next cron.
fn flush_in_background(cx: &Cx, args: &[Bytes]) -> bool {
    let sync = match args.get(1) {
        Some(a) => eq(a, "sync"),
        None => !cx.cfg().lazyfree_lazy_user_flush,
    };
    !sync || cx.can_block()
}

pub fn flushdb(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    flush_mode(args)?;
    let db = cx.db_index();
    cx.eng.touch_all_watched(Some(db as u16));
    cx.eng.clients.flushed();
    // RedisTimeSeries forgets every indexed series on any flush, RediSearch every index.
    cx.eng.ts.index.clear();
    cx.eng.search.flush();
    let removed = cx.eng.dbs[db].len() as u64;
    let reg = cx.eng.hub.templates.clone();
    if flush_in_background(cx, args) {
        reg.deferred(|| cx.eng.dbs[db].clear());
    } else {
        cx.eng.dbs[db].clear();
    }
    cx.eng.sig.dirty += removed;
    cx.force_propagate();
    Ok(Reply::Ok)
}

pub fn flushall(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    flush_mode(args)?;
    let removed: u64 = cx.eng.dbs.iter().map(|d| d.len() as u64).sum();
    // RediSearch's `onFlush`: every index and dictionary goes.
    cx.eng.search.flush();
    let reg = cx.eng.hub.templates.clone();
    if flush_in_background(cx, args) {
        reg.deferred(|| cx.eng.flush_all());
    } else {
        cx.eng.flush_all();
    }
    cx.eng.sig.dirty += removed;
    cx.eng.persist.kill_rdb(cx.now);
    // With save points configured the empty dataset is saved right away (`rdbSave`).
    if !cx.cfg().save.is_empty() {
        cx.eng.reset_dirty();
        cx.eng.lastsave = cx.now / 1000;
    }
    cx.force_propagate();
    Ok(Reply::Ok)
}

pub fn time(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    let us = now_us();
    Ok(Reply::Array(vec![Reply::bulk_str((us / 1_000_000).to_string()), Reply::bulk_str((us % 1_000_000).to_string())]))
}

/// AUTH queued in a transaction, as EXEC runs it (outside one the connection layer handles it):
/// the database's users are checked and the client continues as the one authenticated.
pub fn auth_in_exec(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let (user, pass) = match args.len() {
        2 => (None, &args[1]),
        3 => (Some(&args[1][..]), &args[2]),
        _ => return Err(SYNTAX),
    };
    let acl = cx.eng.acl.clone();
    let acl = acl.read();
    match acl.authenticate(user, pass) {
        Ok(u) => {
            cx.client.user = u;
            cx.client.authenticated = true;
            Ok(Reply::Ok)
        }
        Err(e) => {
            if e == crate::acl::AuthFailure::WrongPass {
                acl.log_auth_failure(cx.client, &args[0], user.unwrap_or(b"default"));
            }
            Err(Error::msg(e.to_string()))
        }
    }
}

pub fn lastsave(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(Reply::Int(cx.eng.lastsave))
}

pub fn role(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    let r = cx.eng.host.as_ref().map(|h| h.replication()).unwrap_or_default();
    Ok(if cx.eng.replica {
        let m = r.master.unwrap_or_default();
        let offset = if m.state == "connected" { m.offset as i64 } else { -1 };
        Reply::Array(vec![Reply::bulk("slave"), Reply::bulk_str(m.host), Reply::Int(m.port as i64), Reply::bulk(m.state), Reply::Int(offset)])
    } else {
        let replicas = r
            .replicas
            .into_iter()
            .map(|s| Reply::Array(vec![Reply::bulk_str(s.ip), Reply::bulk_str(s.port.to_string()), Reply::bulk_str(s.offset.to_string())]))
            .collect();
        Reply::Array(vec![Reply::bulk("master"), Reply::Int(r.offset as i64), Reply::Array(replicas)])
    })
}

pub fn readonly(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    cx.client.readonly = true;
    Ok(Reply::Ok)
}

pub fn readwrite(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    cx.client.readonly = false;
    Ok(Reply::Ok)
}

pub fn asking(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(Reply::Ok)
}

/// CRC16 (XMODEM) hash slot of a key, honouring `{hash tags}`.
pub fn key_slot(key: &[u8]) -> u16 {
    let k = match key.iter().position(|&c| c == b'{') {
        Some(s) => match key[s + 1..].iter().position(|&c| c == b'}') {
            Some(0) | None => key,
            Some(e) => &key[s + 1..s + 1 + e],
        },
        None => key,
    };
    crc16(k) & 16383
}

pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

pub fn cluster_disabled(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Err(Error::new("ERR This instance has cluster support disabled"))
}

/// HELLO [protover [AUTH username password] [SETNAME clientname]]. Authentication is checked by
/// the server layer before this runs.
pub fn hello(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let mut resp = cx.client.resp;
    let mut name = None;
    if args.len() > 1 {
        let v = crate::util::parse_i64(&args[1]).ok_or(Error::new("ERR Protocol version is not an integer or out of range"))?;
        if !(2..=3).contains(&v) {
            return Err(Error::new("NOPROTO unsupported protocol version"));
        }
        resp = v as u8;
        let mut i = 2;
        while i < args.len() {
            if eq(&args[i], "auth") && i + 2 < args.len() {
                i += 3;
            } else if eq(&args[i], "setname") && i + 1 < args.len() {
                super::client::validate_name(&args[i + 1])?;
                name = Some(args[i + 1].clone());
                i += 2;
            } else {
                return Err(Error::msg(format!("ERR Syntax error in HELLO option '{}'", redis_proto::escape(&args[i]))));
            }
        }
    }
    cx.client.resp = resp;
    if let Some(n) = name {
        cx.client.name = if n.is_empty() { None } else { Some(n) };
    }
    Ok(Reply::Map(vec![
        (Reply::bulk("server"), Reply::bulk("redis")),
        (Reply::bulk("version"), Reply::bulk(REDIS_VERSION)),
        (Reply::bulk("proto"), Reply::Int(resp as i64)),
        (Reply::bulk("id"), Reply::Int(cx.client.id as i64)),
        (Reply::bulk("mode"), Reply::bulk("standalone")),
        (Reply::bulk("role"), Reply::bulk(if cx.eng.replica { "replica" } else { "master" })),
        (Reply::bulk("modules"), loaded_modules(cx.eng)),
    ]))
}

pub fn reset(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    cx.client.multi = None;
    cx.client.himport = None;
    let id = cx.client.id;
    cx.eng.unwatch_all(id);
    super::pubsub::unsubscribe_all(cx);
    cx.eng.hub.remove_monitor(id);
    cx.client.monitor = false;
    cx.client.db = 0;
    cx.client.resp = 2;
    cx.client.name = None;
    cx.client.no_touch = false;
    cx.client.no_evict = false;
    cx.client.readonly = false;
    cx.client.reply_mode = ReplyMode::On;
    cx.eng.clients.disable_tracking(id);
    Ok(Reply::status("RESET"))
}

pub fn quit(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(Reply::Ok)
}

/// `configGetCommand`: the matches gathered in a dict of their own (case-insensitive, first
/// name wins), exact names looked up (a rehash step of `configs`), patterns matched against
/// every visible config in the `configs` dict's order; answered in the matches' order.
pub fn config_get(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    // The primary this server follows with REPLICAOF (platform replicas follow no Redis primary).
    let master = cx.eng.host.as_ref().and_then(|h| h.replication().master).filter(|m| !m.managed).map(|m| format!("{} {}", m.host, m.port));
    let known = |name: &[u8]| std::str::from_utf8(name).ok().and_then(crate::config::find);
    let mut matches = crate::sdict::ServerDict::new(true);
    for pat in &args[2..] {
        if !pat.iter().any(|c| b"[*?".contains(c)) {
            if matches.find(pat).is_some() {
                continue;
            }
            if cx.eng.config_dict.find(pat) && known(pat).is_some() {
                matches.add(pat);
            }
            continue;
        }
        for key in cx.eng.config_dict.dict().iter() {
            let Some(p) = known(key) else { continue };
            if p.flags & crate::config::HIDDEN != 0 || matches.find(key).is_some() {
                continue;
            }
            if crate::util::glob_match(pat, key, true) {
                matches.add(key);
            }
        }
    }
    let mut out = Vec::new();
    for name in matches.iter() {
        let name = String::from_utf8_lossy(name).into_owned();
        let Some((_, mut v)) = cx.eng.cfg.get(name.as_bytes()).into_iter().next() else { continue };
        if name.eq_ignore_ascii_case("replicaof") || name.eq_ignore_ascii_case("slaveof") {
            v = master.clone().unwrap_or_default();
        }
        out.push((Reply::bulk_str(name), Reply::bulk_str(v)));
    }
    Ok(Reply::Map(out))
}

/// `allowProtectedAction`: `yes`, or `local` for a loopback connection.
pub(crate) fn protected_allowed(setting: u8, client: &crate::client::Client) -> bool {
    setting == crate::config::PROTECTED_YES
        || (setting == crate::config::PROTECTED_LOCAL
            && client.addr.rsplit_once(':').and_then(|(ip, _)| ip.trim_matches(['[', ']']).parse::<std::net::IpAddr>().ok()).is_some_and(|ip| ip.is_loopback()))
}

pub fn config_set(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if !(args.len() - 2).is_multiple_of(2) {
        return Err(SYNTAX);
    }
    // `lookupConfig` for every name (rehash steps), as Redis finds them all first.
    for kv in args[2..].chunks(2) {
        cx.eng.config_dict.find(&kv[0]);
    }
    let pairs: Vec<(String, String)> =
        args[2..].chunks(2).map(|kv| (String::from_utf8_lossy(&kv[0]).into_owned(), String::from_utf8_lossy(&kv[1]).into_owned())).collect();
    let pairs: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let protected_ok = protected_allowed(cx.eng.cfg.enable_protected_configs, cx.client);
    let before = cx.eng.cfg.clone();
    let set = cx.eng.cfg.set_many(&pairs, protected_ok).map_err(|e| Error::msg(format!("ERR {e}")))?;
    // What the apply step does beyond storing the value.
    for name in set {
        match name {
            "requirepass" => {
                let pass = cx.eng.cfg.requirepass.clone();
                cx.eng.acl.write().set_default_password(pass.as_deref().map(str::as_bytes));
            }
            "acllog-max-len" => cx.eng.acl.read().log.lock().max_len = cx.eng.cfg.acllog_max_len as usize,
            "appendonly" if cx.eng.cfg.appendonly != before.appendonly => cx.eng.set_appendonly(cx.eng.cfg.appendonly, cx.now, cx.mode == crate::engine::Mode::Exec),
            "search-workers" => {
                let n = cx.eng.cfg.num("search-workers");
                cx.eng.search.set_vec_mode(n);
            }
            _ => {}
        }
    }
    cx.eng.hub.set_limits(&cx.eng.cfg);
    Ok(Reply::Ok)
}

pub fn config_resetstat(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    cx.eng.reset_stats();
    Ok(Reply::Ok)
}

/// CONFIG REWRITE: like a Redis started without a config file (INFO's `config_file` is empty);
/// CONFIG SET values persist with the database anyway.
pub fn config_rewrite(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Err(Error::new("ERR The server is running without a config file"))
}

pub fn config_help(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(help(&[
        "CONFIG <subcommand> [<arg> [value] [opt] ...]. Subcommands are:",
        "GET <pattern>",
        "    Return parameters matching the glob-like <pattern> and their values.",
        "SET <directive> <value>",
        "    Set the configuration <directive> to <value>.",
        "RESETSTAT",
        "    Reset statistics reported by the INFO command.",
        "REWRITE",
        "    Rewrite the configuration file.",
        "HELP",
        "    Print this help.",
    ]))
}

pub fn command_count(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(Reply::Int(table().all.iter().filter(|c| visible(cx, c)).count() as i64))
}

/// `commandVisibleForClient`: internal commands only for internal connections.
fn visible(cx: &Cx, c: &Command) -> bool {
    !c.has(INTERNAL) || cx.client.internal
}

pub fn command_list(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let filter = if args.len() == 5 && eq(&args[2], "filterby") {
        Some((args[3].clone(), args[4].clone()))
    } else if args.len() == 2 {
        None
    } else {
        return Err(SYNTAX);
    };
    if let Some((by, v)) = &filter
        && eq(by, "module")
    {
        // `moduleGetHandleByName`, once per listing.
        cx.eng.modules.find(v);
    }
    let mut out = Vec::new();
    for e in super::order::commands() {
        let c = e.cmd;
        let names: Vec<(String, &Command)> = std::iter::once((c.name.to_string(), c))
            .filter(|_| visible(cx, c))
            .chain(e.subs.iter().map(|s| (format!("{}|{}", c.name, s.name), *s)))
            .collect();
        for (n, cmd) in names {
            let keep = match &filter {
                None => true,
                Some((by, v)) if eq(by, "pattern") => crate::util::glob_match(v, n.as_bytes(), true),
                Some((by, v)) if eq(by, "aclcat") => {
                    // The subcommand's own categories.
                    let cat = String::from_utf8_lossy(v).to_lowercase();
                    cmd.categories().contains(&cat.as_str())
                }
                Some((by, v)) if eq(by, "module") => super::module_of(c).is_some_and(|m| m.as_bytes().eq_ignore_ascii_case(v)),
                Some(_) => return Err(SYNTAX),
            };
            if keep {
                out.push(Reply::bulk_str(n));
            }
        }
    }
    Ok(Reply::Array(out))
}

fn info_entry(c: &Command, full: &str) -> Reply {
    let meta = super::meta::get(full);
    Reply::Array(vec![
        Reply::bulk_str(full.to_string()),
        Reply::Int(c.arity as i64),
        Reply::Set(c.flag_names().into_iter().map(Reply::status).collect()),
        Reply::Int(c.first_key as i64),
        Reply::Int(c.last_key as i64),
        Reply::Int(c.key_step as i64),
        Reply::Set(c.categories().into_iter().map(|s| Reply::status(format!("@{s}"))).collect()),
        meta.map_or(Reply::Array(vec![]), |m| m.tips()),
        meta.map_or(Reply::Array(vec![]), |m| m.key_specs()),
        Reply::Array(super::order::subs_of(c).iter().map(|s| info_entry(s, &format!("{}|{}", c.name, s.name))).collect()),
    ])
}

pub fn command_all(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(Reply::Array(super::order::commands().iter().map(|e| e.cmd).filter(|c| visible(cx, c)).map(|c| info_entry(c, c.name)).collect()))
}

pub fn command_info(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() == 2 {
        return command_all(cx, args);
    }
    Ok(Reply::Array(
        args[2..]
            .iter()
            .map(|n| match lookup_full(n) {
                Some((c, full)) if visible(cx, c) => info_entry(c, &full),
                _ => Reply::Nil,
            })
            .collect(),
    ))
}

/// `lookupCommandBySds`: `name` or `container|sub`.
fn lookup_full(name: &[u8]) -> Option<(&'static Command, String)> {
    let mut parts = name.split(|&b| b == b'|');
    let base = lookup(parts.next()?)?;
    match (parts.next(), parts.next()) {
        (None, _) => Some((base, base.name.to_string())),
        (Some(sub), None) if !base.subs.is_empty() => {
            let s = lookup_sub(base, sub)?;
            Some((s, format!("{}|{}", base.name, s.name)))
        }
        _ => None,
    }
}

/// Redis' own docs for the command, with only the subcommands roll has.
fn docs_entry(c: &Command, full: &str) -> Reply {
    if let Some(Reply::Map(fields)) = super::meta::get(full).and_then(|m| m.docs.as_ref()) {
        let fields = fields
            .iter()
            .map(|(k, v)| match (k, v) {
                (Reply::Bulk(name), Reply::Map(_)) if &name[..] == b"subcommands" => (
                    k.clone(),
                    Reply::Map(super::order::subs_of(c).iter().map(|s| {
                        let sub = format!("{}|{}", c.name, s.name);
                        (Reply::bulk_str(sub.clone()), docs_entry(s, &sub))
                    }).collect()),
                ),
                _ => (k.clone(), v.clone()),
            })
            .collect();
        return Reply::Map(fields);
    }
    let mut m = vec![
        (Reply::bulk("summary"), Reply::bulk(c.summary)),
        (Reply::bulk("since"), Reply::bulk(c.since)),
        (Reply::bulk("group"), Reply::bulk(c.group.name())),
    ];
    if !c.subs.is_empty() {
        m.push((
            Reply::bulk("subcommands"),
            Reply::Map(super::order::subs_of(c).iter().map(|s| {
                let sub = format!("{}|{}", c.name, s.name);
                (Reply::bulk_str(sub.clone()), docs_entry(s, &sub))
            }).collect()),
        ));
    }
    Reply::Map(m)
}

pub fn command_docs(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let list: Vec<(&Command, String)> = if args.len() == 2 {
        super::order::commands().iter().map(|e| e.cmd).filter(|c| visible(cx, c)).map(|c| (c, c.name.to_string())).collect()
    } else {
        args[2..].iter().filter_map(|n| lookup_full(n)).filter(|(c, _)| visible(cx, c)).collect()
    };
    Ok(Reply::Map(list.into_iter().map(|(c, full)| (Reply::bulk_str(full.clone()), docs_entry(c, &full))).collect()))
}

/// `lookupCommand` on `COMMAND GETKEYS`'s arguments: a container without a subcommand argument
/// is the container itself.
fn getkeys_cmd(cx: &Cx, args: &[Bytes]) -> Result<Vec<(usize, u16)>, Error> {
    let sub = &args[2..];
    let invalid = || Error::new("ERR Invalid command specified");
    let base = lookup(&sub[0]).ok_or_else(invalid)?;
    let (cmd, full) = if sub.len() == 1 || base.subs.is_empty() {
        (base, base.name.to_string())
    } else {
        let s = lookup_sub(base, &sub[1]).ok_or_else(invalid)?;
        (s, format!("{}|{}", base.name, s.name))
    };
    if !visible(cx, cmd) {
        return Err(invalid());
    }
    if !super::keyspec::has_keys(cmd, &full) {
        return Err(Error::new("ERR The command has no key arguments"));
    }
    if !cmd.check_arity(sub.len()) {
        return Err(Error::new("ERR Invalid number of arguments specified for command"));
    }
    let keys = super::keyspec::keys_with_flags(cmd, &full, sub);
    if keys.is_empty() && !cmd.has(NO_MANDATORY_KEYS) {
        return Err(Error::new("ERR Invalid arguments specified for command"));
    }
    Ok(keys)
}

pub fn command_getkeys(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let keys = getkeys_cmd(cx, args)?;
    Ok(Reply::Array(keys.into_iter().map(|(i, _)| Reply::Bulk(args[2 + i].clone())).collect()))
}

pub fn command_getkeysandflags(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let keys = getkeys_cmd(cx, args)?;
    Ok(Reply::Array(
        keys.into_iter()
            .map(|(i, f)| {
                let flags = super::keyspec::flag_names(f).into_iter().map(Reply::status).collect();
                Reply::Array(vec![Reply::Bulk(args[2 + i].clone()), Reply::Set(flags)])
            })
            .collect(),
    ))
}

pub fn command_help(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(help(&[
        "COMMAND <subcommand> [<arg> [value] [opt] ...]. Subcommands are:",
        "(no subcommand)",
        "    Return details about all Redis commands.",
        "COUNT",
        "    Return the total number of commands in this Redis server.",
        "LIST",
        "    Return a list of all commands in this Redis server.",
        "INFO [<command-name> ...]",
        "    Return details about multiple Redis commands.",
        "    If no command names are given, documentation details for all",
        "    commands are returned.",
        "DOCS [<command-name> ...]",
        "    Return documentation details about multiple Redis commands.",
        "    If no command names are given, documentation details for all",
        "    commands are returned.",
        "GETKEYS <full-command>",
        "    Return the keys from a full Redis command.",
        "GETKEYSANDFLAGS <full-command>",
        "    Return the keys and the access flags from a full Redis command.",
        "HELP",
        "    Print this help.",
    ]))
}

/// MEMORY USAGE key [SAMPLES n]...: `kvobjAllocSize`, found without expiring it (`dbFind`).
pub fn memory_usage(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let mut j = 3;
    while j < args.len() {
        if !(eq(&args[j], "samples") && j + 1 < args.len()) {
            return Err(SYNTAX);
        }
        if int(&args[j + 1])? < 0 {
            return Err(SYNTAX);
        }
        j += 2;
    }
    let key = &args[2];
    let Some(e) = cx.eng.dbs[cx.client.db].keys.get(key) else { return Ok(Reply::Nil) };
    // A template hash also carries its share of the template.
    let share = match e.value() {
        crate::value::ValueRef::Hash(h) => h.template().map_or(0, |t| {
            let keys = t.registry().map_or(1, |r| r.keys_of(t).max(1));
            t.mem() / keys as usize
        }),
        // A series also carries its share of the label index (`IndexMemUsage`).
        crate::value::ValueRef::Ts(t) => cx.eng.ts.index.mem_usage(&t.key.b),
        _ => 0,
    };
    Ok(Reply::Int((crate::object::usage(&e, key.len()) + share) as i64))
}

/// MEMORY STATS: `getMemoryOverheadData` in Redis' order. roll's allocator reports exactly
/// what it holds, so the allocator's own fragmentation stays at 1 / 0 and the RSS overhead
/// shows as the allocator-rss and total fragmentation; ratios are C floats.
pub fn memory_stats(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    let now = crate::util::now_ms();
    let mh = super::info::mem_overhead(cx.eng, now);
    let int = |n: u64| Reply::Int(n as i64);
    let f32d = |v: f32| Reply::Double(v as f64);
    let net = if mh.used > mh.startup { mh.used - mh.startup } else { 1 };
    let ratio = |a: u64, b: u64| if b > 0 { a as f32 / b as f32 } else { 0.0 };
    let mut m = vec![
        (Reply::bulk("peak.allocated"), int(mh.peak)),
        (Reply::bulk("total.allocated"), int(mh.used)),
        (Reply::bulk("startup.allocated"), int(mh.startup)),
        (Reply::bulk("replication.backlog"), int(mh.repl_backlog)),
        (Reply::bulk("replica.fullsync.buffer"), int(0)),
        (Reply::bulk("clients.slaves"), int(0)),
        (Reply::bulk("clients.normal"), int(mh.clients_mem)),
        (Reply::bulk("clients.normal.shared"), int(0)),
        (Reply::bulk("clients.normal.unshared"), int(mh.clients_mem)),
        (Reply::bulk("cluster.links"), int(0)),
        (Reply::bulk("aof.buffer"), int(0)),
        (Reply::bulk("lua.caches"), int(mh.scripts_eval)),
        (Reply::bulk("functions.caches"), int(mh.functions)),
        (Reply::bulk("script.VMs"), int(mh.vm_eval + mh.vm_functions)),
        (Reply::bulk("hash.templates"), int(mh.tmpl_mem)),
    ];
    let mut lut = 0u64;
    for (i, d) in cx.eng.dbs.iter().enumerate() {
        lut += (d.keys.table_bytes() + d.volatile.table_bytes()) as u64;
        if !d.is_empty() {
            m.push((
                Reply::bulk_str(format!("db.{i}")),
                Reply::Map(vec![
                    (Reply::bulk("overhead.hashtable.main"), Reply::Int(d.keys.table_bytes() as i64)),
                    (Reply::bulk("overhead.hashtable.expires"), Reply::Int(d.volatile.table_bytes() as i64)),
                ]),
            ));
        }
    }
    let rss_bytes = mh.rss as i64 - mh.used as i64;
    m.extend([
        (Reply::bulk("overhead.db.hashtable.lut"), int(lut)),
        (Reply::bulk("overhead.db.hashtable.rehashing"), int(0)),
        (Reply::bulk("overhead.total"), int(mh.overhead)),
        (Reply::bulk("db.dict.rehashing.count"), int(0)),
        (Reply::bulk("keys.count"), int(mh.keys)),
        (Reply::bulk("keys.bytes-per-key"), int(mh.dataset.checked_div(mh.keys).unwrap_or(0))),
        (Reply::bulk("dataset.bytes"), int(mh.dataset)),
        (Reply::bulk("dataset.percentage"), f32d(mh.dataset as f32 * 100.0 / net as f32)),
        (Reply::bulk("peak.percentage"), f32d(if mh.peak > 0 { mh.used as f32 * 100.0 / mh.peak as f32 } else { 0.0 })),
        (Reply::bulk("allocator.allocated"), int(mh.used)),
        (Reply::bulk("allocator.active"), int(mh.used)),
        (Reply::bulk("allocator.resident"), int(mh.rss)),
        (Reply::bulk("allocator.muzzy"), int(0)),
        (Reply::bulk("allocator-fragmentation.ratio"), f32d(1.0)),
        (Reply::bulk("allocator-fragmentation.bytes"), int(0)),
        (Reply::bulk("allocator-rss.ratio"), f32d(ratio(mh.rss, mh.used))),
        (Reply::bulk("allocator-rss.bytes"), Reply::Int(rss_bytes)),
        (Reply::bulk("rss-overhead.ratio"), f32d(1.0)),
        (Reply::bulk("rss-overhead.bytes"), int(0)),
        (Reply::bulk("fragmentation"), f32d(ratio(mh.rss, mh.used))),
        (Reply::bulk("fragmentation.bytes"), Reply::Int(rss_bytes)),
    ]);
    Ok(Reply::Map(m))
}

/// MEMORY DOCTOR (`getMemoryDoctorReport`).
pub fn memory_doctor(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    let now = crate::util::now_ms();
    let mh = super::info::mem_overhead(cx.eng, now);
    const MB: u64 = 1024 * 1024;
    let mut issues: Vec<&str> = Vec::new();
    let empty = mh.used < 5 * MB;
    if !empty {
        if mh.peak as f32 / mh.used as f32 > 1.5 {
            issues.push(" * Peak memory: In the past this instance used more than 150% the memory that is currently using. The allocator is normally not able to release memory after a peak, so you can expect to see a big fragmentation ratio, however this is actually harmless and is only due to the memory peak, and if the Redis instance Resident Set Size (RSS) is currently bigger than expected, the memory will be used as soon as you fill the Redis instance with more data. If the memory peak was only occasional and you want to try to reclaim memory, please try the MEMORY PURGE command, otherwise the only other option is to shutdown and restart the instance.\n\n");
        }
        let (frag, frag_bytes) = (mh.rss as f32 / mh.used as f32, mh.rss as i64 - mh.used as i64);
        if frag > 1.4 && frag_bytes > 10 << 20 {
            issues.push(" * High total RSS: This instance has a memory fragmentation and RSS overhead greater than 1.4 (this means that the Resident Set Size of the Redis process is much larger than the sum of the logical allocations Redis performed). This problem is usually due either to a large peak memory (check if there is a peak memory entry above in the report) or may result from a workload that causes the allocator to fragment memory a lot. If the problem is a large peak memory, then there is no issue. Otherwise, make sure you are using the Jemalloc allocator and not the default libc malloc. Note: The currently used allocator is \"libc\".\n\n");
        }
        // The allocator's own fragmentation is 1 / 0 here; its RSS overhead is the total one.
        if frag > 1.1 && frag_bytes > 10 << 20 {
            issues.push(" * High allocator RSS overhead: This instance has an RSS memory overhead is greater than 1.1 (this means that the Resident Set Size of the allocator is much larger than the sum what the allocator actually holds). This problem is usually due to a large peak memory (check if there is a peak memory entry above in the report), you can try the MEMORY PURGE command to reclaim it.\n\n");
        }
        let clients = cx.eng.clients.by_id.len().max(1) as u64;
        if mh.clients_mem / clients > 1024 * 200 {
            issues.push(" * Big client buffers: The clients output buffers in this instance are greater than 200K per client (on average). This may result from different causes, like Pub/Sub clients subscribed to channels bot not receiving data fast enough, so that data piles on the Redis instance output buffer, or clients sending commands with large replies or very large sequences of commands in the same pipeline. Please use the CLIENT LIST command in order to investigate the issue if it causes problems in your instance, or to understand better why certain clients are using a big amount of memory.\n\n");
        }
        if cx.eng.scripting.cached_scripts() > 1000 {
            issues.push(" * Many scripts: There seem to be many cached scripts in this instance (more than 1000). This may be because scripts are generated and `EVAL`ed, instead of being parameterized (with KEYS and ARGV), `SCRIPT LOAD`ed and `EVALSHA`ed. Unless `SCRIPT FLUSH` is called periodically, the scripts' caches may end up consuming most of your memory.\n\n");
        }
    }
    let report = if empty {
        "Hi Sam, this instance is empty or is using very little memory, my issues detector can't be used in these conditions. Please, leave for your mission on Earth and fill it with some data. The new Sam and I will be back to our programming as soon as I finished rebooting.\n".to_string()
    } else if issues.is_empty() {
        "Hi Sam, I can't find any memory issue in your instance. I can only account for what occurs on this base.\n".to_string()
    } else {
        let mut s = String::from("Sam, I detected a few issues in this Redis instance memory implants:\n\n");
        issues.iter().for_each(|i| s.push_str(i));
        s.push_str("I'm here to keep you safe, Sam. I want to help you.\n");
        s
    };
    Ok(Reply::Verbatim("txt", Bytes::from(report)))
}

pub fn memory_malloc_stats(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(Reply::Verbatim("txt", Bytes::from_static(b"Stats not supported for the current allocator")))
}

pub fn memory_purge(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    cx.eng.rehash(1 << 20);
    Ok(Reply::Ok)
}

pub fn memory_help(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(help(&[
        "MEMORY <subcommand> [<arg> [value] [opt] ...]. Subcommands are:",
        "DOCTOR",
        "    Return memory problems reports.",
        "MALLOC-STATS",
        "    Return internal statistics report from the memory allocator.",
        "PURGE",
        "    Attempt to purge dirty pages for reclamation by the allocator.",
        "STATS",
        "    Return information about the memory usage of the server.",
        "USAGE <key> [SAMPLES <count>]",
        "    Return memory in bytes used by <key> and its value. Nested values are",
        "    sampled up to <count> times (default: 5, 0 means sample all).",
        "HELP",
        "    Print this help.",
    ]))
}

pub fn slowlog_get(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let n = match args.get(2) {
        Some(a) => {
            let v = crate::util::parse_i64(a).ok_or(Error::new("ERR count should be greater than or equal to -1"))?;
            if v < -1 {
                return Err(Error::new("ERR count should be greater than or equal to -1"));
            }
            if v == -1 { usize::MAX } else { v as usize }
        }
        None => 10,
    };
    Ok(Reply::Array(
        cx.eng
            .slowlog
            .iter()
            .take(n)
            .map(|e| {
                Reply::Array(vec![
                    Reply::Int(e.id as i64),
                    Reply::Int(e.at),
                    Reply::Int(e.micros as i64),
                    Reply::Array(e.args.iter().cloned().map(Reply::Bulk).collect()),
                    Reply::bulk(&e.addr),
                    Reply::bulk(&e.name),
                    Reply::Int(e.argc as i64),
                ])
            })
            .collect(),
    ))
}

pub fn slowlog_len(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(Reply::Int(cx.eng.slowlog.len() as i64))
}

pub fn slowlog_reset(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    cx.eng.slowlog.clear();
    Ok(Reply::Ok)
}

pub fn slowlog_help(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(help(&[
        "SLOWLOG <subcommand> [<arg> [value] [opt] ...]. Subcommands are:",
        "GET [<count>]",
        "    Return top <count> entries from the slowlog (default: 10, -1 mean all).",
        "    Entries are made of:",
        "    id, timestamp, time in microseconds, arguments array, client IP and port,",
        "    client name",
        "LEN",
        "    Return the length of the slowlog.",
        "RESET",
        "    Reset the slowlog.",
        "HELP",
        "    Print this help.",
    ]))
}

/// LATENCY LATEST: `[event, time, latency, max]` per event with spikes.
pub fn latency_latest(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(Reply::Array(
        cx.eng
            .latency_monitor
            .latest()
            .into_iter()
            .map(|(e, t, l, m)| Reply::Array(vec![Reply::bulk(e), Reply::Int(t), Reply::Int(l as i64), Reply::Int(m as i64)]))
            .collect(),
    ))
}

/// LATENCY HISTORY event: its `[time, latency]` samples, oldest first.
pub fn latency_history(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    Ok(Reply::Array(
        cx.eng.latency_monitor.history(&args[2]).into_iter().map(|(t, l)| Reply::Array(vec![Reply::Int(t), Reply::Int(l as i64)])).collect(),
    ))
}

/// LATENCY GRAPH event: the ASCII sparkline of its samples.
pub fn latency_graph(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let now = crate::util::now_ms() / 1000;
    match cx.eng.latency_monitor.graph(&args[2], now) {
        Some(g) => Ok(Reply::Verbatim("txt", Bytes::from(g))),
        None => Err(Error::msg(format!("ERR No samples available for event '{}'", redis_proto::escape(&args[2])))),
    }
}

/// LATENCY RESET [event ...]: how many events were dropped.
pub fn latency_reset(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let m = &mut cx.eng.latency_monitor;
    let n = if args.len() == 2 { m.reset(None) } else { args[2..].iter().map(|e| m.reset(Some(e))).sum() };
    Ok(Reply::Int(n as i64))
}

/// LATENCY DOCTOR: the human readable report.
pub fn latency_doctor(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    let cfg = &cx.eng.cfg;
    let ctx = crate::latency::ReportCtx {
        threshold_ms: cfg.latency_monitor_threshold,
        slowlog_slower_than: cfg.slowlog_log_slower_than,
        slowlog_max_len: cfg.slowlog_max_len,
        fsync_always: cfg.get(b"appendfsync").first().is_some_and(|(_, v)| v == "always"),
        hz: cfg.hz,
        now: crate::util::now_ms() / 1000,
        anon_huge_pages: crate::latency::anon_huge_pages_bytes() > 0,
    };
    Ok(Reply::Verbatim("txt", Bytes::from(cx.eng.latency_monitor.doctor(&ctx))))
}

/// `LATENCY HISTOGRAM [command ...]`: every command with a latency histogram (Redis walks
/// its command dict), or each named command (`lookupCommandBySds`: `name` or `container|sub`;
/// unknown names skipped) and a container's subcommands, in argument order.
pub fn latency_histogram(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let stats: std::collections::HashMap<String, &crate::engine::CmdStat> =
        cx.eng.cmdstats.iter().map(|(k, v)| (k.to_ascii_lowercase(), v)).collect();
    // `fillCommandCDF` under the command's declared full name.
    let cdf = |name: &str| -> Option<(Reply, Reply)> {
        let h = stats.get(&name.to_ascii_lowercase())?.latency.as_ref()?;
        let buckets = h.log_cdf().into_iter().map(|(us, n)| (Reply::Int(us), Reply::Int(n as i64))).collect();
        Some((
            Reply::bulk_str(name.to_string()),
            Reply::Map(vec![(Reply::bulk("calls"), Reply::Int(h.total() as i64)), (Reply::bulk("histogram_usec"), Reply::Map(buckets))]),
        ))
    };
    let names = super::order::names();
    let mut out = Vec::new();
    if args.len() == 2 {
        // `latencyAllCommandsFillCDF`: the command table's order, under the declared names.
        let recorded = super::order::in_order(cx.eng.cmdstats.iter().filter(|(_, v)| v.latency.is_some()).map(|(k, _)| (k.as_str(), ())));
        out.extend(recorded.into_iter().filter_map(|(n, _)| cdf(super::order::position(n).map_or(n, |i| names[i].as_str()))));
        return Ok(Reply::Map(out));
    }
    // `latencySpecificCommandsFillCDF`: each command found, then its subcommands.
    for a in &args[2..] {
        let mut parts = a.split(|&b| b == b'|');
        let (Some(base), sub, None) = (parts.next(), parts.next(), parts.next()) else { continue };
        let Some(mut cmd) = super::lookup(base) else { continue };
        if let Some(sub) = sub {
            let Some(s) = super::lookup_sub(cmd, sub) else { continue };
            cmd = s;
        }
        let full = crate::acl::full_name(cmd).unwrap_or(cmd.name);
        let Some(at) = super::order::position(full) else { continue };
        out.extend(cdf(&names[at]));
        if !cmd.subs.is_empty() {
            let prefix = format!("{}|", names[at]);
            out.extend(names[at + 1..].iter().take_while(|n| n.starts_with(&prefix)).filter_map(|n| cdf(n)));
        }
    }
    Ok(Reply::Map(out))
}

pub fn latency_help(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(help(&[
        "LATENCY <subcommand> [<arg> [value] [opt] ...]. Subcommands are:",
        "DOCTOR",
        "    Return a human readable latency analysis report.",
        "GRAPH <event>",
        "    Return an ASCII latency graph for the <event> class.",
        "HISTORY <event>",
        "    Return time-latency samples for the <event> class.",
        "LATEST",
        "    Return the latest latency samples for all events.",
        "RESET [<event> ...]",
        "    Reset latency data of one or more <event> classes.",
        "    (default: reset all data for all event classes)",
        "HISTOGRAM [COMMAND ...]",
        "    Return a cumulative distribution of latencies in the format of a histogram for the specified command names.",
        "    If no commands are specified then all histograms are replied.",
        "HELP",
        "    Print this help.",
    ]))
}

/// `addReplyLoadedModules`.
fn loaded_modules(eng: &crate::engine::Engine) -> Reply {
    Reply::Array(
        eng.modules
            .modules()
            .map(|m| {
                Reply::Map(vec![
                    (Reply::bulk("name"), Reply::bulk(m.name)),
                    (Reply::bulk("ver"), Reply::Int(m.ver)),
                    (Reply::bulk("path"), Reply::bulk(m.path)),
                    (Reply::bulk("args"), Reply::Array(vec![])),
                ])
            })
            .collect(),
    )
}

pub fn module_list(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(loaded_modules(cx.eng))
}

pub fn module_denied(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Err(Error::new("ERR MODULE commands are disabled on this server"))
}

pub fn module_help(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(help(&[
        "MODULE <subcommand> [<arg> [value] [opt] ...]. Subcommands are:",
        "LIST",
        "    Return a list of loaded modules.",
        "LOAD <path> [<arg> ...]",
        "    Load a module library from <path>, passing to it any optional arguments.",
        "LOADEX <path> [[CONFIG NAME VALUE] [CONFIG NAME VALUE]] [ARGS ...]",
        "    Load a module library from <path>, while passing it module configurations and optional arguments.",
        "UNLOAD <name>",
        "    Unload a module.",
        "HELP",
        "    Print this help.",
    ]))
}


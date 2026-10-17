//! Connection and server commands that a shard engine can answer itself. Commands that need the
//! server layer (AUTH, CLIENT LIST/KILL, replication, cluster…) are declared here too, so COMMAND
//! and ACL know them, but are intercepted by `redis-server` before reaching an engine.

use super::*;
use crate::client::ReplyMode;
use crate::util::now_us;
use crate::value::Value;
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
    cmd!("auth", -2, CONN_FLAGS, 0, 0, 0, server_only, Connection),
    cmd!("monitor", 1, ADMIN | NOSCRIPT | LOADING | STALE, 0, 0, 0, server_only, Server),
    cmd!("wait", 3, BLOCKING, 0, 0, 0, server_only, Generic),
    cmd!("waitaof", 4, BLOCKING, 0, 0, 0, server_only, Generic),
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
    cmd!("debug", -2, ADMIN | NOSCRIPT | LOADING | STALE | DANGEROUS, 0, 0, 0, debug, Server),
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
    cmd!("latest", 2, ADMIN | NOSCRIPT | LOADING | STALE, 0, 0, 0, latency_empty, Server),
    cmd!("history", 3, ADMIN | NOSCRIPT | LOADING | STALE, 0, 0, 0, latency_empty, Server),
    cmd!("reset", -2, ADMIN | NOSCRIPT | LOADING | STALE, 0, 0, 0, latency_reset, Server),
    cmd!("doctor", 2, ADMIN | NOSCRIPT | LOADING | STALE, 0, 0, 0, latency_doctor, Server),
    cmd!("graph", 3, ADMIN | NOSCRIPT | LOADING | STALE, 0, 0, 0, latency_empty, Server),
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

fn db_arg(cx: &Cx, a: &[u8], not_int: &'static str) -> Result<usize, Error> {
    let n = crate::util::parse_i64(a).ok_or(Error::new(not_int))?;
    if n < 0 || n as usize >= cx.eng.dbs.len() {
        return Err(Error::new("ERR DB index is out of range"));
    }
    Ok(n as usize)
}

pub fn select(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    cx.client.db = db_arg(cx, &args[1], "ERR value is not an integer or out of range")?;
    Ok(Reply::Ok)
}

pub fn swapdb(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let a = db_arg(cx, &args[1], "ERR invalid first DB index")?;
    let b = db_arg(cx, &args[2], "ERR invalid second DB index")?;
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

pub fn flushdb(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    flush_mode(args)?;
    let db = cx.db_index();
    cx.eng.touch_all_watched(Some(db as u16));
    cx.eng.clients.flushed();
    let removed = cx.eng.dbs[db].len() as u64;
    cx.eng.dbs[db].clear();
    cx.eng.sig.dirty += removed;
    cx.force_propagate();
    Ok(Reply::Ok)
}

pub fn flushall(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    flush_mode(args)?;
    let removed: u64 = cx.eng.dbs.iter().map(|d| d.len() as u64).sum();
    cx.eng.flush_all();
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
                return Err(Error::msg(format!("ERR Syntax error in HELLO option '{}'", String::from_utf8_lossy(&args[i]))));
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
        (Reply::bulk("modules"), Reply::Array(vec![])),
    ]))
}

pub fn reset(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    cx.client.multi = None;
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

pub fn config_get(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    // The primary this server follows with REPLICAOF (platform replicas follow no Redis primary).
    let master = cx.eng.host.as_ref().and_then(|h| h.replication().master).filter(|m| !m.managed).map(|m| format!("{} {}", m.host, m.port));
    for pat in &args[2..] {
        for (k, mut v) in cx.eng.cfg.get(pat) {
            if seen.insert(k.to_ascii_lowercase()) {
                if k.eq_ignore_ascii_case("replicaof") || k.eq_ignore_ascii_case("slaveof") {
                    v = master.clone().unwrap_or_default();
                }
                out.push((Reply::bulk_str(k), Reply::bulk_str(v)));
            }
        }
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
            _ => {}
        }
    }
    Ok(Reply::Ok)
}

pub fn config_resetstat(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    cx.eng.reset_stats();
    Ok(Reply::Ok)
}

pub fn config_rewrite(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    // Configuration is persisted by the platform with the database itself.
    Ok(Reply::Ok)
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

pub fn command_count(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(Reply::Int(table().all.len() as i64))
}

pub fn command_list(_cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let filter = if args.len() == 5 && eq(&args[2], "filterby") {
        Some((args[3].clone(), args[4].clone()))
    } else if args.len() == 2 {
        None
    } else {
        return Err(SYNTAX);
    };
    let mut out = Vec::new();
    for c in &table().all {
        let names: Vec<String> =
            std::iter::once(c.name.to_string()).chain(c.subs.iter().map(|s| format!("{}|{}", c.name, s.name))).collect();
        for n in names {
            let keep = match &filter {
                None => true,
                Some((by, v)) if eq(by, "pattern") => crate::util::glob_match(v, n.as_bytes(), true),
                Some((by, v)) if eq(by, "aclcat") => {
                    let cat = String::from_utf8_lossy(v).to_lowercase();
                    c.categories().contains(&cat.as_str())
                }
                Some((by, v)) if eq(by, "module") => {
                    let _ = v;
                    false
                }
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
        Reply::Array(c.subs.iter().map(|s| info_entry(s, &format!("{}|{}", c.name, s.name))).collect()),
    ])
}

pub fn command_all(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(Reply::Array(table().all.iter().map(|c| info_entry(c, c.name)).collect()))
}

pub fn command_info(_cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() == 2 {
        return Ok(Reply::Array(table().all.iter().map(|c| info_entry(c, c.name)).collect()));
    }
    Ok(Reply::Array(
        args[2..]
            .iter()
            .map(|n| match lookup_full(n) {
                Some((c, full)) => info_entry(c, &full),
                None => Reply::Nil,
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
                    Reply::Map(c.subs.iter().map(|s| {
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
            Reply::Map(c.subs.iter().map(|s| {
                let sub = format!("{}|{}", c.name, s.name);
                (Reply::bulk_str(sub.clone()), docs_entry(s, &sub))
            }).collect()),
        ));
    }
    Reply::Map(m)
}

pub fn command_docs(_cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let list: Vec<(&Command, String)> = if args.len() == 2 {
        table().all.iter().map(|c| (*c, c.name.to_string())).collect()
    } else {
        args[2..].iter().filter_map(|n| lookup_full(n)).collect()
    };
    Ok(Reply::Map(list.into_iter().map(|(c, full)| (Reply::bulk_str(full.clone()), docs_entry(c, &full))).collect()))
}

/// `lookupCommand` on `COMMAND GETKEYS`'s arguments: a container without a subcommand argument
/// is the container itself.
fn getkeys_cmd(args: &[Bytes]) -> Result<Vec<(usize, u16)>, Error> {
    let sub = &args[2..];
    let invalid = || Error::new("ERR Invalid command specified");
    let base = lookup(&sub[0]).ok_or_else(invalid)?;
    let (cmd, full) = if sub.len() == 1 || base.subs.is_empty() {
        (base, base.name.to_string())
    } else {
        let s = lookup_sub(base, &sub[1]).ok_or_else(invalid)?;
        (s, format!("{}|{}", base.name, s.name))
    };
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

pub fn command_getkeys(_cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let keys = getkeys_cmd(args)?;
    Ok(Reply::Array(keys.into_iter().map(|(i, _)| Reply::Bulk(args[2 + i].clone())).collect()))
}

pub fn command_getkeysandflags(_cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let keys = getkeys_cmd(args)?;
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
        "DOCS [<command-name> ...]",
        "    Return documentation details about multiple Redis commands.",
        "GETKEYS <full-command>",
        "    Return the keys from a full Redis command.",
        "GETKEYSANDFLAGS <full-command>",
        "    Return the keys and the access flags from a full Redis command.",
        "HELP",
        "    Print this help.",
    ]))
}

pub fn memory_usage(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if args.len() > 3 && !(args.len() == 5 && eq(&args[3], "samples")) {
        return Err(SYNTAX);
    }
    if args.len() == 5 {
        int(&args[4])?;
    }
    let key = &args[2];
    Ok(match cx.peek(key) {
        Some(e) => Reply::Int((crate::value::KEY_OVERHEAD + key.len().saturating_sub(22) + e.value.mem()) as i64),
        None => Reply::Nil,
    })
}

pub fn memory_stats(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    let used = cx.eng.used_memory() as i64;
    let keys = cx.eng.total_keys() as i64;
    let mut m = vec![
        (Reply::bulk("peak.allocated"), Reply::Int(used)),
        (Reply::bulk("total.allocated"), Reply::Int(used)),
        (Reply::bulk("startup.allocated"), Reply::Int(0)),
        (Reply::bulk("overhead.total"), Reply::Int(cx.eng.overhead as i64)),
        (Reply::bulk("keys.count"), Reply::Int(keys)),
        (Reply::bulk("keys.bytes-per-key"), Reply::Int(if keys > 0 { used / keys } else { 0 })),
        (Reply::bulk("dataset.bytes"), Reply::Int(used)),
    ];
    for (i, d) in cx.eng.dbs.iter().enumerate() {
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
    Ok(Reply::Map(m))
}

pub fn memory_doctor(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(Reply::Verbatim("txt", Bytes::from_static(b"Hi Sam, I can't find any memory issue in your instance. I can only account for what occurs on this base.")))
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

pub fn latency_empty(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(Reply::Array(vec![]))
}

pub fn latency_reset(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(Reply::Int(0))
}

pub fn latency_doctor(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(Reply::Verbatim("txt", Bytes::from_static(b"Dave, no latency spike was observed during the lifetime of this Redis instance, not in the slightest bit. I honestly think you ought to sleep better.\n")))
}

pub fn latency_histogram(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let mut out = Vec::new();
    for (name, s) in &cx.eng.cmdstats {
        if args.len() > 2 && !args[2..].iter().any(|a| eq(a, name)) {
            continue;
        }
        out.push((
            Reply::bulk(name),
            Reply::Map(vec![(Reply::bulk("calls"), Reply::Int(s.calls as i64)), (Reply::bulk("histogram_usec"), Reply::Map(vec![]))]),
        ));
    }
    Ok(Reply::Map(out))
}

pub fn latency_help(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(help(&[
        "LATENCY <subcommand> [<arg> [value] [opt] ...]. Subcommands are:",
        "DOCTOR",
        "GRAPH <event>",
        "HISTORY <event>",
        "LATEST",
        "RESET [<event> ...]",
        "HISTOGRAM [COMMAND ...]",
        "HELP",
    ]))
}

pub fn module_list(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(Reply::Array(vec![]))
}

pub fn module_denied(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Err(Error::new("ERR MODULE commands are disabled on this server"))
}

pub fn module_help(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(help(&["MODULE <subcommand> [<arg> [value] [opt] ...]. Subcommands are:", "LIST", "HELP"]))
}

/// DEBUG: the subset tests and tools rely on.
pub fn debug(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if !protected_allowed(cx.cfg().enable_debug_command, cx.client) {
        return Err(Error::new("ERR DEBUG command not allowed. If the enable-debug-command option is set to \"local\", you can run it from a local connection, otherwise you need to set this option in the configuration file, and then restart the server."));
    }
    let sub = &args[1];
    if eq(sub, "help") {
        return Ok(help(&["DEBUG <subcommand> [<arg> [value] [opt] ...]. Subcommands are:", "OBJECT <key>", "SLEEP <seconds>", "SET-ACTIVE-EXPIRE <0|1>", "PROTOCOL <type>", "JMAP", "HELP"]));
    }
    if eq(sub, "object") && args.len() == 3 {
        let Some(e) = cx.peek(&args[2]) else { return Err(NO_KEY) };
        let ser = e.value.mem();
        let extra = match &e.value {
            Value::List(l) => format!(" ql_nodes:1 ql_avg_node:{:.2} ql_listpack_max:-2 ql_compressed:0 ql_uncompressed_size:{}", l.len() as f64, ser),
            _ => String::new(),
        };
        return Ok(Reply::status(format!(
            "Value at:0x0 refcount:1 encoding:{} serializedlength:{} lru:{} lru_seconds_idle:0{}",
            e.value.encoding(),
            ser.max(1),
            e.lru & crate::engine::LRU_CLOCK_MAX,
            extra
        )));
    }
    if eq(sub, "sleep") && args.len() == 3 {
        let secs = float(&args[2])?;
        std::thread::sleep(std::time::Duration::from_secs_f64(secs.clamp(0.0, 10.0)));
        return Ok(Reply::Ok);
    }
    if eq(sub, "set-active-expire") || eq(sub, "quicklist-packed-threshold") || eq(sub, "set-skip-checksum-validation") || eq(sub, "replybuffer") {
        return Ok(Reply::Ok);
    }
    if eq(sub, "jmap") || eq(sub, "log") {
        return Ok(Reply::Ok);
    }
    if eq(sub, "change-repl-id") {
        return Ok(Reply::Ok);
    }
    if eq(sub, "protocol") && args.len() == 3 {
        let t = String::from_utf8_lossy(&args[2]).to_lowercase();
        return Ok(match t.as_str() {
            "string" => Reply::bulk("Hello World"),
            "integer" => Reply::Int(12345),
            "double" => Reply::Double("3.141".parse().unwrap()),
            "bignum" => Reply::BigNum("1234567999999999999999999999999999999".into()),
            "null" => Reply::Nil,
            "array" => Reply::Array((0..3).map(Reply::Int).collect()),
            "set" => Reply::Set((0..3).map(Reply::Int).collect()),
            "map" => Reply::Map((0..3).map(|i| (Reply::Int(i), Reply::Bool(i == 1))).collect()),
            "attrib" => Reply::bulk("Some real reply following the attribute"),
            "push" => Reply::Push(vec![Reply::bulk("server-cpu-usage"), Reply::Int(42)]),
            "true" => Reply::Bool(true),
            "false" => Reply::Bool(false),
            "verbatim" => Reply::Verbatim("txt", Bytes::from_static(b"This is a verbatim\nstring")),
            _ => return Err(Error::new("ERR Wrong protocol type name. Please use one of the following: string|integer|double|bignum|null|array|set|map|attrib|push|verbatim|true|false")),
        });
    }
    Err(Error::msg(format!("ERR unknown subcommand '{}'. Try DEBUG HELP.", String::from_utf8_lossy(sub))))
}


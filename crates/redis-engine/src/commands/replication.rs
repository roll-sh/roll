//! Replication commands an ordinary client can run: REPLCONF's options (before SYNC/PSYNC,
//! which the server handles), REPLICAOF / SLAVEOF and ROLE. The replication links themselves
//! live in the server crate; REPLICAOF hands it a [`ServerAction`].

use super::*;
use redis_proto::Reply as R;

fn not_int() -> R {
    R::error("ERR value is not an integer or out of range")
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn parse_i64(b: &[u8]) -> Option<i64> {
    crate::util::parse_i64(b)
}

/// What a connection said about itself with REPLCONF before SYNC/PSYNC.
#[derive(Clone, Debug, Default)]
pub struct ReplicaConf {
    pub listening_port: i64,
    pub ip: Option<String>,
    pub eof: bool,
    pub psync2: bool,
    pub rdb_only: bool,
    /// `rdb-filter-only`: the snapshot holds only functions (`Some(true)`) or nothing at all.
    pub filter: Option<bool>,
}

/// REPLCONF from a connection that isn't a replica (yet). `None`: no reply (ACK, GETACK).
pub fn replconf(conf: &mut ReplicaConf, args: &[Bytes]) -> Option<Reply> {
    if args.len().is_multiple_of(2) {
        return Some(Reply::error("ERR syntax error"));
    }
    let range01 = |v: &[u8]| match parse_i64(v) {
        None => Err(not_int()),
        Some(n) if !(0..=1).contains(&n) => Err(Reply::error("ERR value is out of range, value must between 0 and 1")),
        Some(n) => Ok(n == 1),
    };
    for kv in args[1..].chunks(2) {
        let (k, v) = (&kv[0], &kv[1]);
        if eq(k, "listening-port") {
            let Some(p) = parse_i64(v) else { return Some(not_int()) };
            conf.listening_port = p;
        } else if eq(k, "ip-address") {
            if v.len() >= 256 {
                return Some(Reply::error(format!("ERR REPLCONF ip-address provided by replica instance is too long: {} bytes", v.len())));
            }
            conf.ip = Some(lossy(v));
        } else if eq(k, "capa") {
            if eq(v, "eof") {
                conf.eof = true;
            } else if eq(v, "psync2") {
                conf.psync2 = true;
            }
        } else if eq(k, "compression") {
            if parse_i64(v).is_none() {
                return Some(not_int());
            }
            return Some(Reply::error("ERR Master has not enabled replication compression"));
        } else if eq(k, "ack") || eq(k, "getack") {
            return None;
        } else if eq(k, "rdb-only") {
            match range01(v) {
                Ok(b) => conf.rdb_only = b,
                Err(e) => return Some(e),
            }
        } else if eq(k, "rdb-filter-only") {
            let Some(filters) = redis_proto::split_args(v) else { return Some(Reply::error("ERR Missing rdb-filter-only values")) };
            let mut functions = false;
            for f in filters {
                if eq(&f, "functions") {
                    functions = true;
                } else {
                    return Some(Reply::error(format!("ERR Unsupported rdb-filter-only option: {}", lossy(&f))));
                }
            }
            conf.filter = Some(functions);
        } else if eq(k, "rdb-channel") || eq(k, "rdb-no-compress") || eq(k, "rdb-no-checksum") {
            if let Err(e) = range01(v) {
                return Some(e);
            }
        } else if eq(k, "main-ch-client-id") {
            let Some(id) = parse_i64(v) else { return Some(not_int()) };
            return Some(Reply::error(format!("ERR Unrecognized RDB client id: {id}")));
        } else {
            return Some(Reply::error(format!("ERR Unrecognized REPLCONF option: {}", lossy(k))));
        }
    }
    Some(Reply::Ok)
}

/// What the server does after a command, outside the engine.
pub enum ServerAction {
    /// EVAL under SCRIPT DEBUG YES: the debugging session runs on `child`, a copy of this
    /// engine (Redis forks), as `client`; this connection is the child's from now on, and closes
    /// when the session ends.
    /// `prefix`: what the client had pending in the child at the fork (EXEC's reply so far).
    LdbFork { child: Box<crate::engine::Engine>, client: Box<crate::client::Client>, args: Vec<Bytes>, prefix: Bytes },
    /// Follow a Redis primary.
    ReplicaOf { host: String, port: u16 },
    /// Stop following and become writable.
    NoOne,
    /// DEBUG RESTART / CRASH-AND-RECOVER and the crash subcommands: restart the database after
    /// `delay_ms` (its clients are disconnected, the command gets no reply).
    Restart { delay_ms: u64 },
    /// DEBUG CHANGE-REPL-ID: a new replication ID, and no previous one.
    ChangeReplId,
    /// DEBUG REPLICATE: send these arguments to the replicas as a command.
    Replicate(Vec<Bytes>),
}

/// REPLCONF run by the engine (inside MULTI/EXEC): validated, with nothing to remember.
pub fn replconf_cmd(_cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    Ok(replconf(&mut ReplicaConf::default(), args).unwrap_or(R::Raw(Bytes::new())))
}

/// REPLICAOF host port | NO ONE.
pub fn replicaof(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let repl = cx.eng.host.as_ref().map(|h| h.replication()).unwrap_or_default();
    if !repl.failover_state.is_empty() && repl.failover_state != "no-failover" {
        return Err(Error::new("ERR REPLICAOF not allowed while failing over."));
    }
    let master = repl.master;
    if master.as_ref().is_some_and(|m| m.managed) {
        return Err(Error::new("ERR this replica is managed by the platform: promote or detach it from the dashboard or API"));
    }
    if eq(&args[1], "no") && eq(&args[2], "one") {
        if cx.eng.replica {
            cx.eng.server_actions.push(ServerAction::NoOne);
        }
        return Ok(R::Ok);
    }
    let Some(port) = parse_i64(&args[2]).filter(|p| (0..=65535).contains(p)) else { return Err(Error::new("ERR Invalid master port")) };
    let host = lossy(&args[1]);
    if master.as_ref().is_some_and(|m| m.host.eq_ignore_ascii_case(&host) && m.port as i64 == port) {
        return Ok(R::status("OK Already connected to specified master"));
    }
    // Redis' disconnectAllBlockedClients: blocked commands end, their clients close.
    cx.eng.unblock_all_for_role_change();
    cx.eng.server_actions.push(ServerAction::ReplicaOf { host, port: port as u16 });
    Ok(R::Ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &[&str]) -> Vec<Bytes> {
        s.iter().map(|a| Bytes::copy_from_slice(a.as_bytes())).collect()
    }

    #[test]
    fn replconf_options() {
        let mut c = ReplicaConf::default();
        assert_eq!(replconf(&mut c, &args(&["replconf", "listening-port", "6380", "capa", "eof", "capa", "psync2"])), Some(R::Ok));
        assert!(c.eof && c.psync2 && c.listening_port == 6380);
        assert_eq!(replconf(&mut c, &args(&["replconf", "ack", "10"])), None);
        assert!(replconf(&mut c, &args(&["replconf", "x"])).is_some_and(|r| r.is_error()));
        assert!(replconf(&mut c, &args(&["replconf", "nope", "1"])).is_some_and(|r| r.is_error()));
        assert_eq!(replconf(&mut c, &args(&["replconf", "rdb-filter-only", "functions"])), Some(R::Ok));
        assert_eq!(c.filter, Some(true));
    }

}

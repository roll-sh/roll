//! MIGRATE: move keys to another Redis-compatible server. Like Redis it runs synchronously
//! (the shard waits for the target), over a cached connection per target (reused for 10 seconds,
//! SELECT sent only when the database changes), and replicates as `DEL` of the keys the target
//! acknowledged.

use super::*;
use crate::cmd;
use crate::notify;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

pub static COMMANDS: &[Command] = &[
    Command { since: "2.6.0", ..cmd!("migrate", -6, WRITE, 3, 3, 1, keys = migrate_keys, migrate, Generic) },
];

/// `MIGRATE host port key|"" db timeout [COPY] [REPLACE] [AUTH pw] [AUTH2 user pw] [KEYS k...]`.
fn migrate_keys(args: &[Bytes]) -> Vec<usize> {
    let mut j = 6;
    while j < args.len() {
        if eq(&args[j], "auth") {
            j += 2;
        } else if eq(&args[j], "auth2") {
            j += 3;
        } else if eq(&args[j], "keys") {
            if args[3].is_empty() {
                return (j + 1..args.len()).collect();
            }
            break;
        } else {
            j += 1;
        }
    }
    vec![3]
}

const CACHE_ITEMS: usize = 64;
const CACHE_TTL: Duration = Duration::from_secs(10);

struct Cached {
    reader: BufReader<TcpStream>,
    last_db: i64,
    last_use: Instant,
}

/// Connections to migration targets, by `host:port`.
#[derive(Default)]
pub struct Sockets(HashMap<String, Cached>);

impl Sockets {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Close connections idle for more than 10 seconds (`migrateCloseTimedoutSockets`).
    pub fn expire(&mut self) {
        self.0.retain(|_, c| c.last_use.elapsed() <= CACHE_TTL);
    }

    fn get(&mut self, name: &str, host: &str, port: &str, timeout: Duration) -> Result<&mut Cached, Error> {
        self.expire();
        if !self.0.contains_key(name) {
            if self.0.len() == CACHE_ITEMS
                && let Some(k) = self.0.keys().next().cloned()
            {
                self.0.remove(&k);
            }
            let stream = match connect(host, port, timeout) {
                Ok(s) => s,
                // A refused connection only shows when writing, as with Redis' non-blocking connect.
                Err(Refused) => return Err(Error::new("IOERR error or timeout writing to target instance")),
                Err(Unreachable) => return Err(Error::new("IOERR error or timeout connecting to the client")),
            };
            self.0.insert(name.to_string(), Cached { reader: BufReader::new(stream), last_db: -1, last_use: Instant::now() });
        }
        let c = self.0.get_mut(name).expect("inserted");
        c.last_use = Instant::now();
        Ok(c)
    }
}

enum ConnectError {
    Refused,
    Unreachable,
}
use ConnectError::*;

fn connect(host: &str, port: &str, timeout: Duration) -> Result<TcpStream, ConnectError> {
    let port: u16 = port.parse().map_err(|_| Unreachable)?;
    let addr = (host, port).to_socket_addrs().map_err(|_| Unreachable)?.next().ok_or(Unreachable)?;
    let s = TcpStream::connect_timeout(&addr, timeout).map_err(|e| match e.kind() {
        std::io::ErrorKind::ConnectionRefused => Refused,
        _ => Unreachable,
    })?;
    let _ = s.set_nodelay(true);
    s.set_read_timeout(Some(timeout)).map_err(|_| Unreachable)?;
    s.set_write_timeout(Some(timeout)).map_err(|_| Unreachable)?;
    Ok(s)
}

fn bulk(out: &mut Vec<u8>, a: &[u8]) {
    out.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
    out.extend_from_slice(a);
    out.extend_from_slice(b"\r\n");
}

fn command(out: &mut Vec<u8>, args: &[&[u8]]) {
    out.extend_from_slice(format!("*{}\r\n", args.len()).as_bytes());
    for a in args {
        bulk(out, a);
    }
}

/// One reply line without its CRLF (`connSyncReadLine`), `None` on error or timeout.
fn read_line(r: &mut BufReader<TcpStream>) -> Option<Vec<u8>> {
    let mut line = Vec::new();
    match r.read_until(b'\n', &mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => {
            while matches!(line.last(), Some(b'\n' | b'\r')) {
                line.pop();
            }
            line.truncate(1023);
            Some(line)
        }
    }
}

fn is_timeout(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock)
}

fn migrate(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let (mut copy, mut replace) = (false, false);
    let (mut user, mut pass): (Option<Bytes>, Option<Bytes>) = (None, None);
    let (mut first, mut count) = (3, 1);
    let mut j = 6;
    while j < args.len() {
        let more = args.len() - 1 - j;
        if eq(&args[j], "copy") {
            copy = true;
        } else if eq(&args[j], "replace") {
            replace = true;
        } else if eq(&args[j], "auth") {
            if more == 0 {
                return Err(SYNTAX);
            }
            j += 1;
            pass = Some(args[j].clone());
        } else if eq(&args[j], "auth2") {
            if more < 2 {
                return Err(SYNTAX);
            }
            user = Some(args[j + 1].clone());
            pass = Some(args[j + 2].clone());
            j += 2;
        } else if eq(&args[j], "keys") {
            if !args[3].is_empty() {
                return Err(Error::new("ERR When using MIGRATE KEYS option, the key argument must be set to the empty string"));
            }
            first = j + 1;
            count = args.len() - j - 1;
            break;
        } else {
            return Err(SYNTAX);
        }
        j += 1;
    }
    let timeout = crate::util::parse_i64(&args[5]).ok_or(NOT_INT)?;
    let db = crate::util::parse_i64(&args[4]).ok_or(NOT_INT)?;
    let timeout = Duration::from_millis(if timeout <= 0 { 1000 } else { timeout as u64 });

    // Serialize what exists now (expired keys are skipped).
    let now = cx.now;
    let mut items: Vec<(Bytes, i64, Vec<u8>)> = Vec::new();
    for k in &args[first..first + count] {
        if cx.get_entry(k).is_some() {
            let mut e = cx.eng.dbs[cx.client.db].keys.get_mut(k).expect("found");
            if let Some(v) = e.aggregate_mut() {
                v.prepare_save();
            }
            let ttl = if e.expire() == 0 { 0 } else { (e.expire() - now).max(1) };
            items.push((k.clone(), ttl, crate::rdb::dump(e.as_ref().value(), now)));
        }
    }
    if items.is_empty() {
        return Ok(Reply::status("NOKEY"));
    }

    let host = String::from_utf8_lossy(&args[1]).into_owned();
    let port = String::from_utf8_lossy(&args[2]).into_owned();
    let name = format!("{host}:{port}");
    let mut may_retry = true;
    loop {
        let sock = cx.eng.migrate_sockets.get(&name, &host, &port, timeout)?;
        let mut out = Vec::new();
        if let Some(p) = &pass {
            match &user {
                Some(u) => command(&mut out, &[b"AUTH", u, p]),
                None => command(&mut out, &[b"AUTH", p]),
            }
        }
        let select = sock.last_db != db;
        if select {
            command(&mut out, &[b"SELECT", db.to_string().as_bytes()]);
        }
        for (k, ttl, payload) in &items {
            let ttl = ttl.to_string();
            if replace {
                command(&mut out, &[b"RESTORE", k, ttl.as_bytes(), payload, b"REPLACE"]);
            } else {
                command(&mut out, &[b"RESTORE", k, ttl.as_bytes(), payload]);
            }
        }

        let write = sock.reader.get_mut().write_all(&out);
        let (mut timed_out, mut write_error) = (false, false);
        let mut auth_reply = None;
        let mut select_reply = None;
        let mut socket_error = false;
        let mut acked: Vec<usize> = Vec::new();
        let mut target_error: Option<String> = None;
        let mut read = 0;
        match write {
            Err(e) => {
                write_error = true;
                timed_out = is_timeout(&e);
                socket_error = true;
            }
            Ok(()) => {
                if pass.is_some() {
                    auth_reply = read_line(&mut sock.reader);
                    socket_error |= auth_reply.is_none();
                }
                if !socket_error && select {
                    select_reply = read_line(&mut sock.reader);
                    socket_error |= select_reply.is_none();
                }
                if !socket_error {
                    for i in 0..items.len() {
                        let Some(line) = read_line(&mut sock.reader) else {
                            socket_error = true;
                            break;
                        };
                        read += 1;
                        let failed = [&auth_reply, &select_reply, &Some(line.clone())]
                            .into_iter()
                            .flatten()
                            .find(|l| l.first() == Some(&b'-'))
                            .cloned();
                        match failed {
                            Some(err) => {
                                if target_error.is_none() {
                                    sock.last_db = -1;
                                    target_error = Some(String::from_utf8_lossy(&err[1..]).into_owned());
                                }
                            }
                            None => acked.push(i),
                        }
                    }
                }
            }
        }
        // Retry once on a fresh connection when nothing was processed (a stale cached socket).
        let nothing_read = read == 0 && auth_reply.is_none() && select_reply.is_none();
        if socket_error && target_error.is_none() && nothing_read && may_retry && !timed_out {
            cx.eng.migrate_sockets.0.remove(&name);
            may_retry = false;
            continue;
        }
        if socket_error {
            cx.eng.migrate_sockets.0.remove(&name);
        }
        if !copy {
            let mut del = vec![Bytes::from_static(b"DEL")];
            for i in &acked {
                let k = &items[*i].0;
                if cx.delete(k) {
                    cx.notify(notify::GENERIC, "del", k);
                }
                del.push(k.clone());
            }
            if del.len() > 1 {
                cx.propagate_as(del);
            }
        }
        if let Some(e) = target_error {
            return Err(Error::msg(format!("ERR Target instance replied with error: {e}")));
        }
        if socket_error {
            return Err(Error::msg(format!(
                "IOERR error or timeout {} to target instance",
                if write_error { "writing" } else { "reading" }
            )));
        }
        if let Some(s) = cx.eng.migrate_sockets.0.get_mut(&name) {
            s.last_db = db;
        }
        return Ok(Reply::Ok);
    }
}

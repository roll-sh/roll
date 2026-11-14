//! Raw-protocol cases whose replies are recorded from PostgreSQL 18.6
//! (`tests/data/protocol_cases.out`): what the server says to messages drivers don't send
//! (bad format codes, trailing bytes, garbage types and lengths, odd startup packets,
//! protocol 3.2 and its cancel key, COPY's lost synchronization), as short strings: "1"
//! (ParseComplete), "D:[\"1\"]", "C:SELECT 1", "ZI", an error's fields, "closed" (the server
//! closed the connection), "quiet" (nothing more came). Record them again from a server that
//! trusts the connection:
//!
//!     PROTOCOL_RECORD=127.0.0.1:5432 cargo test -p pg-engine --test wire protocol_cases

#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const QUIET: Duration = Duration::from_millis(400);

pub fn msg(t: u8, body: &[u8]) -> Vec<u8> {
    let mut m = vec![t];
    m.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    m.extend_from_slice(body);
    m
}

/// A message whose length word says `n`, whatever follows.
pub fn rawlen(t: u8, n: i32, body: &[u8]) -> Vec<u8> {
    let mut m = vec![t];
    m.extend_from_slice(&n.to_be_bytes());
    m.extend_from_slice(body);
    m
}

pub fn cstr(s: &str) -> Vec<u8> {
    let mut v = s.as_bytes().to_vec();
    v.push(0);
    v
}

pub fn parse(name: &str, sql: &str, types: &[u32]) -> Vec<u8> {
    let mut b = cstr(name);
    b.extend(cstr(sql));
    b.extend((types.len() as i16).to_be_bytes());
    for t in types {
        b.extend(t.to_be_bytes());
    }
    msg(b'P', &b)
}

pub fn bind(portal: &str, stmt: &str, pformats: &[i16], params: &[Option<&[u8]>], rformats: &[i16]) -> Vec<u8> {
    let mut b = cstr(portal);
    b.extend(cstr(stmt));
    b.extend((pformats.len() as i16).to_be_bytes());
    for f in pformats {
        b.extend(f.to_be_bytes());
    }
    b.extend((params.len() as i16).to_be_bytes());
    for p in params {
        match p {
            None => b.extend((-1i32).to_be_bytes()),
            Some(v) => {
                b.extend((v.len() as i32).to_be_bytes());
                b.extend_from_slice(v);
            }
        }
    }
    b.extend((rformats.len() as i16).to_be_bytes());
    for f in rformats {
        b.extend(f.to_be_bytes());
    }
    msg(b'B', &b)
}

pub fn execute(portal: &str, n: i32) -> Vec<u8> {
    let mut b = cstr(portal);
    b.extend(n.to_be_bytes());
    msg(b'E', &b)
}

pub fn describe(kind: u8, name: &str) -> Vec<u8> {
    let mut b = vec![kind];
    b.extend(cstr(name));
    msg(b'D', &b)
}

pub fn close(kind: u8, name: &str) -> Vec<u8> {
    let mut b = vec![kind];
    b.extend(cstr(name));
    msg(b'C', &b)
}

pub fn query(sql: &str) -> Vec<u8> {
    msg(b'Q', &cstr(sql))
}

pub fn sync() -> Vec<u8> {
    msg(b'S', b"")
}

pub fn flush() -> Vec<u8> {
    msg(b'H', b"")
}

/// A startup packet: protocol `proto`, user postgres, database postgres, then `params`;
/// `tail` after the pairs (its terminator: "\0").
pub fn startup_packet(proto: i32, params: &[(&str, &str)], tail: &[u8]) -> Vec<u8> {
    let mut b = proto.to_be_bytes().to_vec();
    for (k, v) in [("user", "postgres"), ("database", "postgres")].iter().chain(params) {
        b.extend(cstr(k));
        b.extend(cstr(v));
    }
    b.extend_from_slice(tail);
    let mut m = ((b.len() + 4) as i32).to_be_bytes().to_vec();
    m.extend(b);
    m
}

pub const P30: i32 = 3 << 16;
pub const P32: i32 = (3 << 16) | 2;

/// A message the server sent, as the cases record it.
fn compact(t: u8, b: &[u8]) -> String {
    let str_at = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    match t {
        b'E' | b'N' => {
            let mut f = Vec::new();
            for &k in b"SCMDHPWFLR" {
                if let Some(p) = b.split(|c| *c == 0).find(|p| p.first() == Some(&k)) {
                    f.push(format!("{}={}", k as char, str_at(&p[1..])));
                }
            }
            format!("{}{{{}}}", t as char, f.join("|"))
        }
        b'Z' => format!("Z{}", b[0] as char),
        b'C' => format!("C:{}", str_at(&b[..b.len() - 1])),
        b'S' => {
            let mut parts = b.split(|c| *c == 0);
            let k = str_at(parts.next().unwrap_or_default());
            let v = str_at(parts.next().unwrap_or_default());
            // (What differs by build and host.)
            match k.as_str() {
                "server_version" | "TimeZone" => format!("S:{k}"),
                _ => format!("S:{k}={v}"),
            }
        }
        b'K' => format!("K:{}", b.len() - 4),
        b'v' => {
            let ver = i32::from_be_bytes(b[..4].try_into().unwrap());
            let n = i32::from_be_bytes(b[4..8].try_into().unwrap()) as usize;
            let names: Vec<String> = b[8..].split(|c| *c == 0).take(n).map(str_at).collect();
            format!("v:{}.{}:{}", ver >> 16, ver & 0xffff, names.join(","))
        }
        b'T' => {
            let n = i16::from_be_bytes([b[0], b[1]]) as usize;
            let (mut at, mut f) = (2, Vec::new());
            for _ in 0..n {
                at += b[at..].iter().position(|c| *c == 0).unwrap() + 1 + 18;
                f.push(i16::from_be_bytes([b[at - 2], b[at - 1]]).to_string());
            }
            format!("T:{}", f.join(","))
        }
        b'D' => {
            let n = i16::from_be_bytes([b[0], b[1]]) as usize;
            let (mut at, mut v) = (2, Vec::new());
            for _ in 0..n {
                let len = i32::from_be_bytes(b[at..at + 4].try_into().unwrap());
                at += 4;
                if len < 0 {
                    v.push("null".to_string());
                } else {
                    v.push(format!("{:?}", str_at(&b[at..at + len as usize])));
                    at += len as usize;
                }
            }
            format!("D:[{}]", v.join(","))
        }
        b'R' => format!("R{}", i32::from_be_bytes(b[..4].try_into().unwrap())),
        b't' => format!("t:{}", i16::from_be_bytes([b[0], b[1]])),
        t => (t as char).to_string(),
    }
}

pub struct Conn {
    s: TcpStream,
    buf: Vec<u8>,
    /// How long a read waits before the server counts as quiet.
    quiet: Duration,
}

enum Got {
    Msg(u8, Vec<u8>),
    Closed,
    Quiet,
}

impl Conn {
    pub fn open(addr: &str, packet: &[u8]) -> Conn {
        let mut s = TcpStream::connect(addr).unwrap();
        s.write_all(packet).unwrap();
        Conn { s, buf: Vec::new(), quiet: QUIET }
    }

    fn fill(&mut self) -> Option<bool> {
        self.s.set_read_timeout(Some(self.quiet)).unwrap();
        let mut b = [0u8; 65536];
        match self.s.read(&mut b) {
            Ok(0) => Some(false),
            Ok(n) => {
                self.buf.extend_from_slice(&b[..n]);
                Some(true)
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => None,
            Err(_) => Some(false),
        }
    }

    fn next(&mut self) -> Got {
        loop {
            if self.buf.len() >= 5 {
                let n = i32::from_be_bytes(self.buf[1..5].try_into().unwrap()) as usize;
                if self.buf.len() > n {
                    let t = self.buf[0];
                    let b = self.buf[5..1 + n].to_vec();
                    self.buf.drain(..1 + n);
                    return Got::Msg(t, b);
                }
            }
            match self.fill() {
                Some(true) => {}
                Some(false) => return Got::Closed,
                None => return Got::Quiet,
            }
        }
    }

    /// One byte (an SSL / GSS request's answer).
    pub fn byte(&mut self) -> String {
        if self.buf.is_empty() {
            match self.fill() {
                Some(true) => {}
                Some(false) => return "closed".into(),
                None => return "quiet".into(),
            }
        }
        let b = self.buf.remove(0);
        (b as char).to_string()
    }

    /// What the server says until ReadyForQuery (or CopyInResponse), closing, or quiet.
    pub fn rest(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        loop {
            match self.next() {
                Got::Closed => {
                    out.push("closed".into());
                    return out;
                }
                Got::Quiet => {
                    out.push("quiet".into());
                    return out;
                }
                Got::Msg(t, b) => {
                    out.push(compact(t, &b));
                    if t == b'Z' || t == b'G' {
                        return out;
                    }
                }
            }
        }
    }

    /// The startup's replies, up to ReadyForQuery, an error and what follows, or closing.
    pub fn startup(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        loop {
            match self.next() {
                Got::Closed => {
                    out.push("closed".into());
                    return out;
                }
                Got::Quiet => {
                    out.push("quiet".into());
                    return out;
                }
                Got::Msg(t, b) => {
                    out.push(compact(t, &b));
                    if t == b'Z' {
                        return out;
                    }
                    if t == b'E' {
                        out.extend(self.rest());
                        return out;
                    }
                }
            }
        }
    }

    pub fn send(&mut self, b: &[u8]) -> bool {
        self.s.write_all(b).is_ok()
    }

    /// A session of protocol `proto`: its BackendKeyData's process id and key.
    pub fn keyed(addr: &str, proto: i32) -> (Conn, i32, Vec<u8>) {
        let mut c = Conn::open(addr, &startup_packet(proto, &[], b"\0"));
        let (mut pid, mut key) = (0, Vec::new());
        loop {
            match c.next() {
                Got::Msg(b'K', b) => {
                    pid = i32::from_be_bytes(b[..4].try_into().unwrap());
                    key = b[4..].to_vec();
                }
                Got::Msg(b'Z', _) | Got::Closed | Got::Quiet => return (c, pid, key),
                Got::Msg(..) => {}
            }
        }
    }
}

/// A session: `setup` simple queries (their replies dropped), then each step sent and what
/// it got, " / " between steps.
pub fn session(addr: &str, setup: &[&str], steps: &[Vec<u8>]) -> String {
    session_as(addr, P30, &[], setup, steps)
}

/// `session` of protocol `proto`, without setup.
pub fn session_at(addr: &str, proto: i32, steps: &[Vec<u8>]) -> String {
    session_as(addr, proto, &[], &[], steps)
}

/// `session` whose startup packet has `params` too, without setup.
pub fn session_params(addr: &str, params: &[(&str, &str)], steps: &[Vec<u8>]) -> String {
    session_as(addr, P30, params, &[], steps)
}

fn session_as(addr: &str, proto: i32, params: &[(&str, &str)], setup: &[&str], steps: &[Vec<u8>]) -> String {
    let mut c = Conn::open(addr, &startup_packet(proto, params, b"\0"));
    let s = c.startup();
    if s.last().map(String::as_str) != Some("ZI") {
        return format!("startup {}", s.join(" "));
    }
    for q in setup {
        c.send(&query(q));
        c.rest();
    }
    let mut out = Vec::new();
    for st in steps {
        if !c.send(st) {
            out.push("send failed".to_string());
            break;
        }
        let r = c.rest();
        let closed = r.last().map(String::as_str) == Some("closed");
        out.push(r.join(" "));
        if closed {
            break;
        }
    }
    out.join(" / ")
}

/// `session` of one extended-protocol batch ending in Sync.
pub fn ext(addr: &str, setup: &[&str], ms: &[Vec<u8>]) -> String {
    let mut b: Vec<u8> = ms.concat();
    b.extend(sync());
    session(addr, setup, &[b])
}

/// The replies to a raw startup packet.
pub fn startup(addr: &str, packet: &[u8]) -> String {
    Conn::open(addr, packet).startup().join(" ")
}

/// Packets answered by a byte (SSLRequest, GSSENCRequest), each sent after the last's
/// 'N', then `then`'s startup replies. An ErrorResponse instead of a byte ends it.
pub fn byte_replies(addr: &str, packets: &[Vec<u8>], then: Option<&[u8]>) -> String {
    let mut c = Conn::open(addr, &packets[0]);
    let mut out = Vec::new();
    for (i, p) in packets.iter().enumerate() {
        if i > 0 {
            c.send(p);
        }
        let b = c.byte();
        if b == "E" {
            c.buf.insert(0, b'E');
            out.extend(c.rest());
            return out.join(" ");
        }
        let n = b == "N";
        out.push(b);
        if !n {
            return out.join(" ");
        }
    }
    // (A request pipelined with what follows it: the server's error after its 'N'.)
    if !c.buf.is_empty() || then.is_none() {
        out.extend(c.rest());
        return out.join(" ");
    }
    if let Some(p) = then {
        c.send(p);
        out.extend(c.startup());
    }
    out.join(" ")
}

pub fn ssl_request(extra: &[u8]) -> Vec<u8> {
    let mut m = ((8 + extra.len()) as i32).to_be_bytes().to_vec();
    m.extend(80877103i32.to_be_bytes());
    m.extend_from_slice(extra);
    m
}

pub fn gss_request() -> Vec<u8> {
    let mut m = 8i32.to_be_bytes().to_vec();
    m.extend(80877104i32.to_be_bytes());
    m
}

/// A session of protocol `proto` runs pg_sleep(2); a CancelRequest `f` makes of its pid and
/// key follows: the key's length, what the cancel connection got, then the session.
pub fn cancel(addr: &str, proto: i32, f: impl Fn(i32, &[u8]) -> Vec<u8>) -> String {
    let (mut c, pid, key) = Conn::keyed(addr, proto);
    c.send(&query("select pg_sleep(2)"));
    std::thread::sleep(Duration::from_millis(300));
    let mut k = TcpStream::connect(addr).unwrap();
    k.write_all(&f(pid, &key)).unwrap();
    let mut got = [0u8; 1];
    k.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let first = match k.read(&mut got) {
        Ok(0) | Err(_) => "closed".to_string(),
        Ok(_) => format!("{}", got[0] as char),
    };
    c.quiet = Duration::from_secs(5);
    format!("K:{} {first} / {}", key.len(), c.rest().join(" "))
}

/// A CancelRequest packet: `pid` and `key`.
pub fn cancel_packet(pid: i32, key: &[u8]) -> Vec<u8> {
    let mut m = ((12 + key.len()) as i32).to_be_bytes().to_vec();
    m.extend(80877102i32.to_be_bytes());
    m.extend(pid.to_be_bytes());
    m.extend_from_slice(key);
    m
}

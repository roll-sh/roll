//! Redis servers outside roll (ElastiCache, Upstash, Redis Cloud, a server of the customer's own):
//! the same guards as Postgres (a public address resolved here; TLS through `rediss://`, verified
//! against the URL's host). Passwords go as `#<sha256>`, so the server never sees them. A dynamic
//! login copies a template user's rules (its commands, keys and channels), and ending it is
//! `ACL DELUSER`, which also closes its connections. Redis has no expiry for a user: a dynamic
//! login ends when roll ends its lease.
//!
//! The server is the tenant's: its replies are read by a small parser that bounds depth, counts
//! and lengths by the bytes present, never by what the server claims.

use super::{TIMEOUT, resolve, tls_config};
use sha2::Digest;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_rustls::rustls::pki_types::ServerName;

/// The most of one reply read (an ACL GETUSER is a few hundred bytes).
const REPLY_MAX: usize = 1 << 20;
const DEPTH_MAX: usize = 8;

pub(super) struct Url {
    host: String,
    port: u16,
    user: Option<String>,
    password: String,
    tls: bool,
}

pub(super) fn parse(url: &str, allow_private: bool) -> Result<Url, String> {
    let bad = || "the URL is rediss://user:password@host:6379".to_string();
    let u = url::Url::parse(url).map_err(|_| bad())?;
    let tls = match u.scheme() {
        "rediss" => true,
        "redis" if allow_private => false,
        "redis" => return Err("roll only sends a password over TLS: use rediss://".into()),
        _ => return Err(bad()),
    };
    let host = match u.host() {
        Some(url::Host::Domain(d)) => d.to_string(),
        Some(url::Host::Ipv4(ip)) => ip.to_string(),
        Some(url::Host::Ipv6(ip)) => ip.to_string(),
        None => return Err(bad()),
    };
    if let Some((k, _)) = u.query_pairs().next() {
        return Err(format!("the URL may not set {k}"));
    }
    if !matches!(u.path(), "" | "/" | "/0") {
        return Err("roll manages users, not a database: drop the path".into());
    }
    let decode = |s: &str| super::mysql::percent_decode(s).ok_or_else(bad);
    let password = decode(u.password().unwrap_or(""))?;
    if password.is_empty() {
        return Err("the URL carries the admin's password".into());
    }
    let user = if u.username().is_empty() { None } else { Some(decode(u.username())?) };
    Ok(Url { host, port: u.port().unwrap_or(6379), user, password, tls })
}

/// A reply, as far as roll reads one.
#[derive(Debug, PartialEq)]
pub(super) enum Reply {
    Status(String),
    Error(String),
    Int(i64),
    Bulk(Vec<u8>),
    Nil,
    Array(Vec<Reply>),
}

/// One reply from the front of `buf`: `Ok(None)` when it's incomplete.
pub(super) fn read_reply(buf: &[u8]) -> Result<Option<(Reply, usize)>, String> {
    let mut p = 0;
    Ok(value(buf, &mut p, 0)?.map(|r| (r, p)))
}

fn line(buf: &[u8], p: &mut usize) -> Option<Vec<u8>> {
    let i = buf.get(*p..)?.windows(2).position(|w| w == b"\r\n")?;
    let l = buf[*p..*p + i].to_vec();
    *p += i + 2;
    Some(l)
}

fn int(l: &[u8]) -> Result<i64, String> {
    std::str::from_utf8(l).ok().and_then(|s| s.parse().ok()).ok_or_else(|| "a malformed reply".to_string())
}

fn value(buf: &[u8], p: &mut usize, depth: usize) -> Result<Option<Reply>, String> {
    if depth > DEPTH_MAX {
        return Err("a reply nested too deep".into());
    }
    let Some(&kind) = buf.get(*p) else { return Ok(None) };
    *p += 1;
    let Some(l) = line(buf, p) else { return Ok(None) };
    Ok(Some(match kind {
        b'+' => Reply::Status(String::from_utf8_lossy(&l).into_owned()),
        b'-' => Reply::Error(String::from_utf8_lossy(&l).into_owned()),
        b':' => Reply::Int(int(&l)?),
        b'_' => Reply::Nil,
        b'$' => match int(&l)? {
            -1 => Reply::Nil,
            n if n < 0 => return Err("a malformed reply".into()),
            n => {
                let n = n as usize;
                // Bounded by what's here, not by what was claimed.
                if buf.len().saturating_sub(*p) < n.saturating_add(2) {
                    return Ok(None);
                }
                let b = buf[*p..*p + n].to_vec();
                *p += n + 2;
                Reply::Bulk(b)
            }
        },
        b'*' | b'%' => match int(&l)? {
            -1 => Reply::Nil,
            n if n < 0 => return Err("a malformed reply".into()),
            n => {
                // A map's pairs are read as a flat array (RESP2's shape).
                let n = if kind == b'%' { n.saturating_mul(2) } else { n } as usize;
                // Every element takes at least 3 bytes: no more than that fit in what's here.
                let room = buf.len().saturating_sub(*p) / 3;
                if n > room {
                    return Ok(None);
                }
                let mut out = Vec::with_capacity(n);
                for _ in 0..n {
                    match value(buf, p, depth + 1)? {
                        Some(v) => out.push(v),
                        None => return Ok(None),
                    }
                }
                Reply::Array(out)
            }
        },
        _ => return Err("a reply roll doesn't read".into()),
    }))
}

fn encode(args: &[&[u8]]) -> Vec<u8> {
    let mut out = format!("*{}\r\n", args.len()).into_bytes();
    for a in args {
        out.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
        out.extend_from_slice(a);
        out.extend_from_slice(b"\r\n");
    }
    out
}

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

pub(super) struct Conn {
    io: Box<dyn Io>,
    buf: Vec<u8>,
}

impl Conn {
    /// A command's reply; an error reply is an error (the server's text, never the arguments).
    async fn call(&mut self, args: &[&[u8]]) -> Result<Reply, String> {
        self.io.write_all(&encode(args)).await.map_err(|e| e.to_string())?;
        loop {
            if let Some((r, n)) = read_reply(&self.buf)? {
                self.buf.drain(..n);
                return match r {
                    Reply::Error(e) => Err(e),
                    r => Ok(r),
                };
            }
            if self.buf.len() >= REPLY_MAX {
                return Err("a reply too big".into());
            }
            let mut chunk = [0u8; 8192];
            let n = self.io.read(&mut chunk).await.map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("the server closed the connection".into());
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }
}

async fn connect(u: &Url, ca: Option<&str>, allow_private: bool) -> Result<Conn, String> {
    let addr = resolve(&u.host, u.port, allow_private).await?;
    let tcp = tokio::net::TcpStream::connect(addr).await.map_err(|e| format!("{}: {e}", u.host))?;
    let io: Box<dyn Io> = if u.tls {
        let name = ServerName::try_from(u.host.clone()).map_err(|_| format!("{} isn't a name TLS can check", u.host))?;
        let tls = tokio_rustls::TlsConnector::from(Arc::new(tls_config(ca)?)).connect(name, tcp).await.map_err(|e| format!("{}: {e}", u.host))?;
        Box::new(tls)
    } else {
        Box::new(tcp)
    };
    let mut c = Conn { io, buf: Vec::new() };
    let auth = match &u.user {
        Some(user) => c.call(&[b"AUTH", user.as_bytes(), u.password.as_bytes()]).await,
        None => c.call(&[b"AUTH", u.password.as_bytes()]).await,
    };
    auth.map_err(|e| format!("{}: {e}", u.host))?;
    Ok(c)
}

pub(super) async fn run<T>(u: Url, ca: Option<String>, allow_private: bool, work: impl AsyncFnOnce(&mut Conn) -> Result<T, String>) -> Result<T, String> {
    tokio::time::timeout(TIMEOUT, async move {
        let mut c = connect(&u, ca.as_deref(), allow_private).await?;
        work(&mut c).await
    })
    .await
    .map_err(|_| "timed out".to_string())?
}

/// `#<sha256 hex>`: the password as Redis keeps it, so it never crosses the wire.
fn hashed(password: &str) -> Vec<u8> {
    let h = sha2::Sha256::digest(password.as_bytes());
    format!("#{}", h.iter().map(|b| format!("{b:02x}")).collect::<String>()).into_bytes()
}

async fn user(c: &mut Conn, name: &str) -> Result<Option<Reply>, String> {
    Ok(match c.call(&[b"ACL", b"GETUSER", name.as_bytes()]).await? {
        Reply::Nil => None,
        r => Some(r),
    })
}

pub(super) async fn check(c: &mut Conn, roles: &[String], group: Option<&str>) -> Result<(), String> {
    for r in roles.iter().map(String::as_str).chain(group) {
        if user(c, r).await?.is_none() {
            return Err(format!("there's no ACL user {r}"));
        }
    }
    Ok(())
}

pub(super) async fn set_password(c: &mut Conn, role: &str, password: &str) -> Result<(), String> {
    c.call(&[b"ACL", b"SETUSER", role.as_bytes(), b"resetpass", &hashed(password)]).await.map(|_| ())
}

/// The template's rules as `ACL SETUSER` takes them (Redis 7 answers strings of rules, Redis 6
/// lists of patterns).
pub(super) fn rules_of(template: &Reply) -> Result<Vec<Vec<u8>>, String> {
    let Reply::Array(fields) = template else { return Err("ACL GETUSER answered something roll doesn't read".into()) };
    let words = |prefix: &[u8], v: &Reply| -> Vec<Vec<u8>> {
        match v {
            Reply::Bulk(s) => s.split(|b| *b == b' ').filter(|w| !w.is_empty()).map(<[u8]>::to_vec).collect(),
            Reply::Array(a) => a.iter().filter_map(|x| if let Reply::Bulk(s) = x { Some([prefix, s].concat()) } else { None }).collect(),
            _ => vec![],
        }
    };
    let mut out = Vec::new();
    for pair in fields.chunks(2) {
        let [Reply::Bulk(key), v] = pair else { continue };
        match key.as_slice() {
            b"commands" => out.extend(words(b"", v)),
            b"keys" => out.extend(words(b"~", v)),
            b"channels" => out.extend(words(b"&", v)),
            b"selectors" if matches!(v, Reply::Array(a) if !a.is_empty()) => return Err("the template user has selectors, which roll doesn't copy: put its rules on the user itself".into()),
            _ => {}
        }
    }
    Ok(out)
}

pub(super) async fn create_login(c: &mut Conn, name: &str, password: &str, group: &str) -> Result<(), String> {
    let template = user(c, group).await?.ok_or_else(|| format!("there's no ACL user {group}"))?;
    let rules = rules_of(&template)?;
    let hash = hashed(password);
    let mut args: Vec<&[u8]> = vec![b"ACL", b"SETUSER", name.as_bytes(), b"reset", b"on", &hash];
    args.extend(rules.iter().map(Vec::as_slice));
    c.call(&args).await.map(|_| ())
}

/// `ACL DELUSER` also closes the user's connections; one that's gone already is fine.
pub(super) async fn drop_login(c: &mut Conn, name: &str) -> Result<(), String> {
    c.call(&[b"ACL", b"DELUSER", name.as_bytes()]).await.map(|_| ())
}

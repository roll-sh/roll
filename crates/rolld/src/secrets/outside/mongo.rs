//! MongoDB servers outside roll (Atlas, a replica set of the customer's own): the same guards as
//! Postgres (public addresses resolved here; TLS verified against each host's name). User
//! management runs on the primary, found with `hello` among the seed hosts only (or, for
//! `mongodb+srv://`, the SRV record's targets, which must sit inside its domain), so a server can't
//! send roll anywhere else. roll logs in with SCRAM-SHA-256; passwords go in `createUser` and
//! `updateUser` over TLS (MongoDB keeps only SCRAM credentials, and redacts them from its logs).
//! A dynamic login is a user with the group role (`role` or `role@db`), dropped at the lease's end;
//! MongoDB has no expiry for a user.
//!
//! The server is the tenant's: its replies are read by a small BSON reader that bounds nesting and
//! every length by the bytes present, a reply is at most 1 MiB, and the SCRAM iteration count it
//! asks for is capped.

use super::{TIMEOUT, resolve, tls_config};
use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_rustls::rustls::pki_types::ServerName;

const REPLY_MAX: usize = 1 << 20;
const DEPTH_MAX: usize = 16;
/// MongoDB's default is 15,000; a server asking for more than this is refused, not obeyed.
const ITERATIONS_MAX: u32 = 1_000_000;
const SEEDS_MAX: usize = 16;

pub(super) struct Url {
    /// Seed hosts and ports (several for a replica set).
    seeds: Vec<(String, u16)>,
    user: String,
    password: String,
    /// The database users live in (`authSource`, `admin` by default).
    db: String,
    tls: bool,
    /// `mongodb+srv://`: the seeds come from this name's SRV record.
    srv: Option<String>,
}

pub(super) fn parse(url: &str, allow_private: bool) -> Result<Url, String> {
    let bad = || "the URL is mongodb+srv://user:password@cluster.example.net or mongodb://user:password@host:27017/?tls=true".to_string();
    let (srv, rest) = match (url.strip_prefix("mongodb+srv://"), url.strip_prefix("mongodb://")) {
        (Some(r), _) => (true, r),
        (_, Some(r)) => (false, r),
        _ => return Err(bad()),
    };
    let (auth, rest) = rest.rsplit_once('@').ok_or_else(bad)?;
    let (user, password) = auth.split_once(':').ok_or_else(bad)?;
    let decode = |s: &str| super::mysql::percent_decode(s).ok_or_else(bad);
    let (user, password) = (decode(user)?, decode(password)?);
    let (hosts, query) = match rest.split_once('/') {
        Some((h, q)) => (h, q.strip_prefix('?').or_else(|| q.split_once('?').map(|(_, q)| q)).unwrap_or("")),
        None => (rest, ""),
    };
    let mut seeds = Vec::new();
    for h in hosts.split(',').filter(|h| !h.is_empty()) {
        let (host, port) = match h.rsplit_once(':') {
            Some((host, port)) if !host.ends_with(']') || host.starts_with('[') => (host, port.parse::<u16>().map_err(|_| bad())?),
            _ => (h, 27017),
        };
        let host = host.trim_start_matches('[').trim_end_matches(']');
        if host.is_empty() || host.contains(['/', '?', '#', '@']) {
            return Err(bad());
        }
        seeds.push((host.to_string(), port));
    }
    if seeds.is_empty() || seeds.len() > SEEDS_MAX || user.is_empty() || (srv && seeds.len() != 1) {
        return Err(bad());
    }
    // `mongodb+srv://` means TLS unless it says otherwise; `mongodb://` only with tls=true.
    let mut tls = srv;
    let mut db = "admin".to_string();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match (k, v) {
            ("tls" | "ssl", "true") => tls = true,
            ("tls" | "ssl", "false") => tls = false,
            ("authSource", v) if !v.is_empty() && v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') => db = v.to_string(),
            // What Atlas's connection strings carry; nothing roll's commands need.
            ("retryWrites" | "w" | "appName" | "replicaSet" | "authMechanism" | "readPreference", _) => {}
            (k, _) => return Err(format!("the URL may set tls and authSource, but not {k}")),
        }
    }
    if !tls && !allow_private {
        return Err("roll only sends a password over TLS: add tls=true".into());
    }
    Ok(Url { srv: srv.then(|| seeds[0].0.clone()), seeds, user, password, db, tls })
}

/// The SRV record's targets, each inside the record's own domain (the spec's rule: a hostile DNS
/// answer can't point roll at someone else's servers).
async fn srv_seeds(name: &str) -> Result<Vec<(String, u16)>, String> {
    let parent = name.split_once('.').map(|(_, p)| p).filter(|p| p.contains('.')).ok_or_else(|| format!("{name} needs at least three labels for mongodb+srv://"))?;
    let resolver = hickory_resolver::TokioResolver::builder_tokio().map_err(|e| e.to_string())?.build().map_err(|e| e.to_string())?;
    let found = resolver.srv_lookup(format!("_mongodb._tcp.{name}.")).await.map_err(|_| format!("{name} has no _mongodb._tcp SRV record"))?;
    let mut out = Vec::new();
    for r in found.answers() {
        if let hickory_resolver::proto::rr::RData::SRV(s) = &r.data {
            let target = s.target.to_ascii().trim_end_matches('.').to_ascii_lowercase();
            if !target.ends_with(&format!(".{}", parent.to_ascii_lowercase())) {
                return Err(format!("{name}'s SRV record names {target}, outside {parent}"));
            }
            out.push((target, s.port));
        }
    }
    out.truncate(SEEDS_MAX);
    if out.is_empty() { Err(format!("{name} has no _mongodb._tcp SRV record")) } else { Ok(out) }
}

// ── BSON, as far as roll writes and reads it ──

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Bson {
    Double(f64),
    Str(String),
    Doc(Vec<(String, Bson)>),
    Array(Vec<Bson>),
    Bin(Vec<u8>),
    Bool(bool),
    Null,
    I32(i32),
    I64(i64),
    /// A type roll skips (object ids, dates, timestamps, decimals…).
    Other,
}

impl Bson {
    fn get<'a>(&'a self, key: &str) -> Option<&'a Bson> {
        match self {
            Bson::Doc(d) => d.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn truthy(&self) -> bool {
        match self {
            Bson::Double(d) => *d != 0.0,
            Bson::I32(i) => *i != 0,
            Bson::I64(i) => *i != 0,
            Bson::Bool(b) => *b,
            _ => false,
        }
    }

    fn str(&self) -> Option<&str> {
        if let Bson::Str(s) = self { Some(s) } else { None }
    }
}

fn doc(fields: Vec<(&str, Bson)>) -> Bson {
    Bson::Doc(fields.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

fn encode(doc: &Bson, out: &mut Vec<u8>) {
    let Bson::Doc(fields) = doc else { unreachable!("a command is a document") };
    let start = out.len();
    out.extend_from_slice(&[0; 4]);
    for (k, v) in fields {
        element(k, v, out);
    }
    out.push(0);
    let len = (out.len() - start) as i32;
    out[start..start + 4].copy_from_slice(&len.to_le_bytes());
}

fn element(k: &str, v: &Bson, out: &mut Vec<u8>) {
    let kind = match v {
        Bson::Double(_) => 1,
        Bson::Str(_) => 2,
        Bson::Doc(_) => 3,
        Bson::Array(_) => 4,
        Bson::Bin(_) => 5,
        Bson::Bool(_) => 8,
        Bson::Null | Bson::Other => 10,
        Bson::I32(_) => 16,
        Bson::I64(_) => 18,
    };
    out.push(kind);
    out.extend_from_slice(k.as_bytes());
    out.push(0);
    match v {
        Bson::Double(d) => out.extend_from_slice(&d.to_le_bytes()),
        Bson::Str(s) => {
            out.extend_from_slice(&((s.len() + 1) as i32).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
            out.push(0);
        }
        Bson::Doc(_) => encode(v, out),
        Bson::Array(items) => encode(&Bson::Doc(items.iter().enumerate().map(|(i, x)| (i.to_string(), x.clone())).collect()), out),
        Bson::Bin(b) => {
            out.extend_from_slice(&(b.len() as i32).to_le_bytes());
            out.push(0);
            out.extend_from_slice(b);
        }
        Bson::Bool(b) => out.push(u8::from(*b)),
        Bson::Null | Bson::Other => {}
        Bson::I32(i) => out.extend_from_slice(&i.to_le_bytes()),
        Bson::I64(i) => out.extend_from_slice(&i.to_le_bytes()),
    }
}

/// One document from `b`, which must hold exactly it.
pub(super) fn decode(b: &[u8]) -> Result<Bson, String> {
    let (d, n) = document(b, 0)?;
    if n != b.len() { Err("a malformed reply".into()) } else { Ok(d) }
}

fn take<'a>(b: &'a [u8], p: &mut usize, n: usize) -> Result<&'a [u8], String> {
    let end = p.checked_add(n).filter(|e| *e <= b.len()).ok_or("a malformed reply")?;
    let s = &b[*p..end];
    *p = end;
    Ok(s)
}

fn len32(b: &[u8], p: &mut usize) -> Result<usize, String> {
    let n = i32::from_le_bytes(take(b, p, 4)?.try_into().expect("4 bytes"));
    usize::try_from(n).map_err(|_| "a malformed reply".to_string())
}

fn cstring(b: &[u8], p: &mut usize) -> Result<String, String> {
    let i = b.get(*p..).and_then(|r| r.iter().position(|c| *c == 0)).ok_or("a malformed reply")?;
    let s = String::from_utf8_lossy(&b[*p..*p + i]).into_owned();
    *p += i + 1;
    Ok(s)
}

/// A document at the front of `b`, and its length; every length is checked against `b`.
fn document(b: &[u8], depth: usize) -> Result<(Bson, usize), String> {
    if depth > DEPTH_MAX {
        return Err("a reply nested too deep".into());
    }
    let mut p = 0;
    let len = len32(b, &mut p)?;
    if len < 5 || len > b.len() || b[len - 1] != 0 {
        return Err("a malformed reply".into());
    }
    let b = &b[..len];
    let mut fields = Vec::new();
    while p < len - 1 {
        let kind = take(b, &mut p, 1)?[0];
        let key = cstring(b, &mut p)?;
        let v = match kind {
            1 => Bson::Double(f64::from_le_bytes(take(b, &mut p, 8)?.try_into().expect("8 bytes"))),
            2 | 13 | 14 => {
                let n = len32(b, &mut p)?;
                let s = take(b, &mut p, n)?;
                if s.last() != Some(&0) {
                    return Err("a malformed reply".into());
                }
                let s = String::from_utf8_lossy(&s[..s.len() - 1]).into_owned();
                if kind == 2 { Bson::Str(s) } else { Bson::Other }
            }
            3 | 4 => {
                let (d, n) = document(&b[p..], depth + 1)?;
                p += n;
                match (kind, d) {
                    (4, Bson::Doc(items)) => Bson::Array(items.into_iter().map(|(_, v)| v).collect()),
                    (_, d) => d,
                }
            }
            5 => {
                let n = len32(b, &mut p)?;
                take(b, &mut p, 1)?;
                Bson::Bin(take(b, &mut p, n)?.to_vec())
            }
            6 | 10 | 0x7f | 0xff => Bson::Null,
            7 => {
                take(b, &mut p, 12)?;
                Bson::Other
            }
            8 => Bson::Bool(take(b, &mut p, 1)?[0] != 0),
            9 | 17 => {
                take(b, &mut p, 8)?;
                Bson::Other
            }
            11 => {
                cstring(b, &mut p)?;
                cstring(b, &mut p)?;
                Bson::Other
            }
            16 => Bson::I32(i32::from_le_bytes(take(b, &mut p, 4)?.try_into().expect("4 bytes"))),
            18 => Bson::I64(i64::from_le_bytes(take(b, &mut p, 8)?.try_into().expect("8 bytes"))),
            19 => {
                take(b, &mut p, 16)?;
                Bson::Other
            }
            _ => return Err("a reply roll doesn't read".into()),
        };
        fields.push((key, v));
    }
    Ok((Bson::Doc(fields), len))
}

// ── the wire ──

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

pub(super) struct Conn {
    io: Box<dyn Io>,
    next: i32,
    db: String,
}

impl Conn {
    /// An OP_MSG command and its reply; `ok: 0` is an error (the server's message).
    async fn call(&mut self, db: &str, cmd: Vec<(&str, Bson)>) -> Result<Bson, String> {
        let mut cmd = cmd;
        cmd.push(("$db", Bson::Str(db.to_string())));
        let mut body = Vec::new();
        encode(&doc(cmd), &mut body);
        self.next += 1;
        let mut msg = Vec::with_capacity(body.len() + 21);
        msg.extend_from_slice(&((body.len() + 21) as i32).to_le_bytes());
        msg.extend_from_slice(&self.next.to_le_bytes());
        msg.extend_from_slice(&0i32.to_le_bytes());
        msg.extend_from_slice(&2013i32.to_le_bytes());
        msg.extend_from_slice(&0u32.to_le_bytes());
        msg.push(0);
        msg.extend_from_slice(&body);
        self.io.write_all(&msg).await.map_err(|e| e.to_string())?;
        let mut head = [0u8; 16];
        self.io.read_exact(&mut head).await.map_err(|e| e.to_string())?;
        let len = usize::try_from(i32::from_le_bytes(head[..4].try_into().expect("4 bytes"))).map_err(|_| "a malformed reply")?;
        if !(21..=REPLY_MAX).contains(&len) || i32::from_le_bytes(head[12..16].try_into().expect("4 bytes")) != 2013 {
            return Err("a reply roll doesn't read".into());
        }
        let mut rest = vec![0u8; len - 16];
        self.io.read_exact(&mut rest).await.map_err(|e| e.to_string())?;
        let flags = u32::from_le_bytes(rest[..4].try_into().expect("4 bytes"));
        let end = if flags & 1 != 0 { rest.len().checked_sub(4).ok_or("a malformed reply")? } else { rest.len() };
        if rest.get(4) != Some(&0) || end < 5 {
            return Err("a reply roll doesn't read".into());
        }
        let reply = decode(&rest[5..end])?;
        if reply.get("ok").is_some_and(Bson::truthy) {
            Ok(reply)
        } else {
            let code = reply.get("code").and_then(|c| if let Bson::I32(c) = c { Some(*c) } else { None }).unwrap_or(0);
            Err(format!("{} (code {code})", reply.get("errmsg").and_then(Bson::str).unwrap_or("refused")))
        }
    }
}

async fn open(host: &str, port: u16, tls: bool, ca: Option<&str>, allow_private: bool) -> Result<Conn, String> {
    let addr = resolve(host, port, allow_private).await?;
    let tcp = tokio::time::timeout(TIMEOUT, tokio::net::TcpStream::connect(addr)).await.map_err(|_| format!("{host}: timed out"))?.map_err(|e| format!("{host}: {e}"))?;
    let io: Box<dyn Io> = if tls {
        let name = ServerName::try_from(host.to_string()).map_err(|_| format!("{host} isn't a name TLS can check"))?;
        Box::new(tokio_rustls::TlsConnector::from(Arc::new(tls_config(ca)?)).connect(name, tcp).await.map_err(|e| format!("{host}: {e}"))?)
    } else {
        Box::new(tcp)
    };
    Ok(Conn { io, next: 0, db: String::new() })
}

/// The primary, among the seeds: the one `hello` says is writable, or the seed another names.
async fn primary(seeds: &[(String, u16)], tls: bool, ca: Option<&str>, allow_private: bool) -> Result<Conn, String> {
    let mut last = "no seed answered".to_string();
    for (host, port) in seeds {
        let mut c = match open(host, *port, tls, ca, allow_private).await {
            Ok(c) => c,
            Err(e) => {
                last = e;
                continue;
            }
        };
        let hello = match c.call("admin", vec![("hello", Bson::I32(1))]).await {
            Ok(h) => h,
            Err(e) => {
                last = format!("{host}: {e}");
                continue;
            }
        };
        if hello.get("isWritablePrimary").is_some_and(Bson::truthy) {
            return Ok(c);
        }
        if let Some(named) = hello.get("primary").and_then(Bson::str) {
            // Only a seed: a server can't send roll anywhere the URL (or its SRV record) didn't.
            let found = seeds.iter().find(|(h, p)| format!("{h}:{p}").eq_ignore_ascii_case(named));
            if let Some((h, p)) = found {
                return open(h, *p, tls, ca, allow_private).await;
            }
            last = format!("{host} names {named} as primary, which isn't in the URL");
        } else {
            last = format!("{host} isn't a primary");
        }
    }
    Err(last)
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("any key length");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

/// SCRAM-SHA-256 (RFC 7677) as MongoDB runs it, checking the server's signature.
async fn login(c: &mut Conn, user: &str, password: &str, db: &str) -> Result<(), String> {
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut raw = [0u8; 24];
    getrandom::fill(&mut raw).map_err(|e| e.to_string())?;
    let nonce = b64.encode(raw);
    let first_bare = format!("n={},r={nonce}", user.replace('=', "=3D").replace(',', "=2C"));
    let start = c
        .call(db, vec![("saslStart", Bson::I32(1)), ("mechanism", Bson::Str("SCRAM-SHA-256".into())), ("payload", Bson::Bin(format!("n,,{first_bare}").into_bytes())), ("options", doc(vec![("skipEmptyExchange", Bson::Bool(true))]))])
        .await?;
    let (Some(Bson::Bin(server_first)), Some(id)) = (start.get("payload"), start.get("conversationId").cloned()) else { return Err("the server didn't start SCRAM".into()) };
    let server_first = String::from_utf8_lossy(server_first).into_owned();
    let field = |k: &str| server_first.split(',').find_map(|p| p.strip_prefix(&format!("{k}=")));
    let (Some(r), Some(s), Some(i)) = (field("r"), field("s"), field("i")) else { return Err("the server's SCRAM reply is malformed".into()) };
    let iterations: u32 = i.parse().map_err(|_| "the server's SCRAM reply is malformed")?;
    if !r.starts_with(&nonce) || !(4096..=ITERATIONS_MAX).contains(&iterations) {
        return Err("the server's SCRAM reply isn't one roll accepts".into());
    }
    let salt = b64.decode(s).map_err(|_| "the server's SCRAM reply is malformed")?;
    let mut salted = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(password.as_bytes(), &salt, iterations, &mut salted);
    let client_key = hmac(&salted, b"Client Key");
    let stored = Sha256::digest(&client_key);
    let without_proof = format!("c=biws,r={r}");
    let auth = format!("{first_bare},{server_first},{without_proof}");
    let signature = hmac(&stored, auth.as_bytes());
    let proof: Vec<u8> = client_key.iter().zip(&signature).map(|(a, b)| a ^ b).collect();
    let fin = c.call(db, vec![("saslContinue", Bson::I32(1)), ("conversationId", id), ("payload", Bson::Bin(format!("{without_proof},p={}", b64.encode(proof)).into_bytes()))]).await?;
    let expected = format!("v={}", b64.encode(hmac(&hmac(&salted, b"Server Key"), auth.as_bytes())));
    match fin.get("payload") {
        Some(Bson::Bin(v)) if v.as_slice() == expected.as_bytes() => Ok(()),
        _ => Err("the server didn't prove it knows the password: not the server the URL names".into()),
    }
}

pub(super) async fn run<T>(u: Url, ca: Option<String>, allow_private: bool, work: impl AsyncFnOnce(&mut Conn) -> Result<T, String>) -> Result<T, String> {
    tokio::time::timeout(TIMEOUT, async move {
        let seeds = match &u.srv {
            Some(name) => srv_seeds(name).await?,
            None => u.seeds.clone(),
        };
        let mut c = primary(&seeds, u.tls, ca.as_deref(), allow_private).await?;
        login(&mut c, &u.user, &u.password, &u.db).await?;
        c.db = u.db.clone();
        work(&mut c).await
    })
    .await
    .map_err(|_| "timed out".to_string())?
}

/// A role as `{role, db}`: `name` in the users' database, or `name@db`.
fn role(name: &str, users_db: &str) -> Bson {
    let (r, db) = name.split_once('@').unwrap_or((name, users_db));
    doc(vec![("role", Bson::Str(r.into())), ("db", Bson::Str(db.into()))])
}

pub(super) async fn check(c: &mut Conn, roles: &[String], group: Option<&str>) -> Result<(), String> {
    let db = c.db.clone();
    for r in roles {
        let found = c.call(&db, vec![("usersInfo", doc(vec![("user", Bson::Str(r.clone())), ("db", Bson::Str(db.clone()))]))]).await?;
        if !matches!(found.get("users"), Some(Bson::Array(u)) if !u.is_empty()) {
            return Err(format!("there's no user {r} in {db}"));
        }
    }
    if let Some(g) = group {
        let found = c.call(&db, vec![("rolesInfo", role(g, &db)), ("showBuiltinRoles", Bson::Bool(true))]).await?;
        if !matches!(found.get("roles"), Some(Bson::Array(r)) if !r.is_empty()) {
            return Err(format!("there's no role {g}"));
        }
    }
    Ok(())
}

pub(super) async fn set_password(c: &mut Conn, user: &str, password: &str) -> Result<(), String> {
    let db = c.db.clone();
    c.call(&db, vec![("updateUser", Bson::Str(user.into())), ("pwd", Bson::Str(password.into())), ("mechanisms", Bson::Array(vec![Bson::Str("SCRAM-SHA-256".into())]))]).await.map(|_| ())
}

pub(super) async fn create_login(c: &mut Conn, name: &str, password: &str, group: &str) -> Result<(), String> {
    let db = c.db.clone();
    let r = role(group, &db);
    c.call(&db, vec![("createUser", Bson::Str(name.into())), ("pwd", Bson::Str(password.into())), ("roles", Bson::Array(vec![r])), ("mechanisms", Bson::Array(vec![Bson::Str("SCRAM-SHA-256".into())]))]).await.map(|_| ())
}

pub(super) async fn drop_login(c: &mut Conn, name: &str) -> Result<(), String> {
    let db = c.db.clone();
    // Its sessions end where the admin may (killAllSessionsByPattern); dropping the user blocks new ones.
    let pattern = Bson::Array(vec![doc(vec![("users", Bson::Array(vec![doc(vec![("user", Bson::Str(name.into())), ("db", Bson::Str(db.clone()))])]))])]);
    let _ = c.call("admin", vec![("killAllSessionsByPattern", pattern)]).await;
    match c.call(&db, vec![("dropUser", Bson::Str(name.into()))]).await {
        Ok(_) => Ok(()),
        // UserNotFound: gone already.
        Err(e) if e.ends_with("(code 11)") => Ok(()),
        Err(e) => Err(e),
    }
}

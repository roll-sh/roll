//! The Postgres wire protocol (v3): startup, SCRAM-SHA-256 authentication, simple and
//! extended query, COPY, cancel requests. One thread per connection: sessions are not
//! `Send`, and a thread with a big stack gives deep queries room before
//! "stack depth limit exceeded".

use crate::auth::{ScramServer, hash_password, parse_verifier};
use crate::engine::Engine;
use crate::error::{Error, Result, code};
use crate::session::{Outcome, Prepared, QueryResult, Session, TxnStatus};
use crate::sql::plan::OutCol;
use crate::types::text::{FormatCtx, format_value};
use bytes::{BufMut, BytesMut};
use std::io::{BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{debug, warn};

const SSL_REQUEST: i32 = 80877103;
const GSSENC_REQUEST: i32 = 80877104;
const CANCEL_REQUEST: i32 = 80877102;
const PROTOCOL_3: i32 = 196608;
const MAX_MESSAGE: usize = 1 << 30;
const THREAD_STACK: usize = 16 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthMethod {
    /// Anyone who names an existing role gets in (local development).
    Trust,
    /// SCRAM-SHA-256 against the role's stored verifier.
    ScramSha256,
}

#[derive(Clone, Debug)]
pub struct ServerOptions {
    pub auth: AuthMethod,
}

/// Accept connections until the listener fails.
pub fn serve(
    engine: Arc<Engine>,
    listener: TcpListener,
    opts: ServerOptions,
) -> std::io::Result<()> {
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "accept failed");
                continue;
            }
        };
        let engine = engine.clone();
        let opts = opts.clone();
        std::thread::Builder::new()
            .name("roll-conn".into())
            .stack_size(THREAD_STACK)
            .spawn(move || {
                let peer = stream.peer_addr().ok();
                if let Err(e) = handle(engine, stream, peer, &opts) {
                    debug!(error = %e, "connection ended");
                }
            })?;
    }
    Ok(())
}

/// Run a server on a background thread (tests, embedding). Returns the bound address.
pub fn spawn(engine: Arc<Engine>, addr: &str, opts: ServerOptions) -> std::io::Result<SocketAddr> {
    let listener = TcpListener::bind(addr)?;
    let local = listener.local_addr()?;
    std::thread::Builder::new()
        .name("roll-accept".into())
        .spawn(move || serve(engine, listener, opts))?;
    Ok(local)
}

// ── framing ──

struct Conn {
    rd: BufReader<TcpStream>,
    wr: TcpStream,
    out: BytesMut,
    /// Where the message being written starts (to drop it if a panic cuts it short).
    partial: Option<usize>,
    /// A FATAL error went out: the session ends.
    fatal: bool,
}

impl Conn {
    fn read_i32(&mut self) -> std::io::Result<i32> {
        let mut b = [0u8; 4];
        self.rd.read_exact(&mut b)?;
        Ok(i32::from_be_bytes(b))
    }

    fn read_body(&mut self, len: i32, header: usize) -> std::io::Result<Vec<u8>> {
        let len = len as usize;
        if len < header || len > MAX_MESSAGE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid message length",
            ));
        }
        let mut body = vec![0u8; len - header];
        self.rd.read_exact(&mut body)?;
        Ok(body)
    }

    /// A regular message: (type, body).
    fn read_message(&mut self) -> std::io::Result<Option<(u8, Vec<u8>)>> {
        let mut t = [0u8; 1];
        if self.rd.read(&mut t)? == 0 {
            return Ok(None);
        }
        let len = self.read_i32()?;
        let body = self.read_body(len, 4)?;
        Ok(Some((t[0], body)))
    }

    fn msg(&mut self, t: u8, body: impl FnOnce(&mut BytesMut)) {
        self.partial = Some(self.out.len());
        self.out.put_u8(t);
        let at = self.out.len();
        self.out.put_i32(0);
        body(&mut self.out);
        let len = (self.out.len() - at) as i32;
        self.out[at..at + 4].copy_from_slice(&len.to_be_bytes());
        self.partial = None;
    }

    /// After a panic: drop a message left half written.
    fn drop_partial(&mut self) {
        if let Some(at) = self.partial.take()
            && at <= self.out.len()
        {
            self.out.truncate(at);
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if !self.out.is_empty() {
            self.wr.write_all(&self.out)?;
            self.out.clear();
        }
        Ok(())
    }

    /// Flush when the buffer gets big (long result sets).
    fn maybe_flush(&mut self) -> std::io::Result<()> {
        if self.out.len() >= 64 << 10 {
            self.flush()
        } else {
            Ok(())
        }
    }

    fn error(&mut self, e: &Error) {
        if e.severity == crate::error::Severity::Fatal {
            self.fatal = true;
        }
        self.msg(b'E', |b| error_fields(b, e, e.severity.as_str()));
    }

    fn notice(&mut self, e: &Error) {
        self.msg(b'N', |b| error_fields(b, e, e.severity.as_str()));
    }

    fn ready(&mut self, status: TxnStatus) -> std::io::Result<()> {
        self.msg(b'Z', |b| b.put_u8(status.byte()));
        self.flush()
    }

    fn parameter_status(&mut self, name: &str, value: &str) {
        self.msg(b'S', |b| {
            cstr(b, name);
            cstr(b, value);
        });
    }

    fn auth(&mut self, kind: i32, data: &[u8]) {
        self.msg(b'R', |b| {
            b.put_i32(kind);
            b.put_slice(data);
        });
    }
}

thread_local! {
    /// The session's client_encoding (sessions run on threads of their own): text goes out
    /// in it and query text comes in in it.
    static CLIENT_ENC: std::cell::Cell<i32> = const { std::cell::Cell::new(crate::types::mbconv::UTF8) };
}

/// Take up the session's client_encoding for the messages that follow.
fn sync_client_encoding(s: &Session) {
    let e = s.state.settings.borrow().get_raw("client_encoding");
    let id = e
        .as_deref()
        .and_then(crate::types::encnames::encoding_id)
        .unwrap_or(crate::types::mbconv::UTF8);
    CLIENT_ENC.with(|c| c.set(id));
}

fn client_enc() -> i32 {
    CLIENT_ENC.with(|c| c.get())
}

/// Server text in the client's encoding (pg_server_to_client).
fn to_client(s: &str) -> Result<std::borrow::Cow<'_, [u8]>> {
    let enc = client_enc();
    if enc == crate::types::mbconv::UTF8 {
        return Ok(std::borrow::Cow::Borrowed(s.as_bytes()));
    }
    crate::types::mbconv::convert(s.as_bytes(), crate::types::mbconv::UTF8, enc)
}

/// Client bytes as server text (pg_client_to_server).
fn from_client(b: &[u8]) -> Result<Vec<u8>> {
    let enc = client_enc();
    if enc == crate::types::mbconv::UTF8 {
        return Ok(b.to_vec());
    }
    Ok(crate::types::mbconv::to_utf8(b, enc)?.into_bytes())
}

/// A message body whose first `n` NUL-terminated strings are converted from the client's
/// encoding (the rest, binary, kept).
fn convert_cstrings(body: &[u8], n: usize) -> Result<Vec<u8>> {
    if client_enc() == crate::types::mbconv::UTF8 {
        return Ok(body.to_vec());
    }
    let mut out = Vec::with_capacity(body.len());
    let mut rest = body;
    for _ in 0..n {
        let Some(end) = rest.iter().position(|b| *b == 0) else {
            break;
        };
        out.extend_from_slice(&from_client(&rest[..end])?);
        out.push(0);
        rest = &rest[end + 1..];
    }
    out.extend_from_slice(rest);
    Ok(out)
}

fn cstr(b: &mut BytesMut, s: &str) {
    // (A message that can't be converted goes out as it is, as elog's would.)
    match to_client(s) {
        Ok(v) => b.put_slice(&v),
        Err(_) => b.put_slice(s.as_bytes()),
    }
    b.put_u8(0);
}

fn error_fields(b: &mut BytesMut, e: &Error, severity: &str) {
    for (t, v) in [
        (b'S', severity),
        (b'V', severity),
        (b'C', e.code),
        (b'M', e.message.as_str()),
    ] {
        b.put_u8(t);
        cstr(b, v);
    }
    if let Some(d) = &e.detail {
        b.put_u8(b'D');
        cstr(b, d);
    }
    if let Some(h) = &e.hint {
        b.put_u8(b'H');
        cstr(b, h);
    }
    if let Some(p) = e.position {
        b.put_u8(b'P');
        cstr(b, &p.to_string());
    }
    if let Some((p, q)) = e.objects.as_ref().and_then(|o| o.internal.as_ref()) {
        b.put_u8(b'p');
        cstr(b, &p.to_string());
        b.put_u8(b'q');
        cstr(b, q);
    }
    if let Some(c) = &e.context {
        b.put_u8(b'W');
        cstr(b, c);
    }
    if let Some(o) = &e.objects {
        for (t, v) in [
            (b's', &o.schema),
            (b't', &o.table),
            (b'c', &o.column),
            (b'd', &o.datatype),
            (b'n', &o.constraint),
        ] {
            if let Some(v) = v {
                b.put_u8(t);
                cstr(b, v);
            }
        }
    }
    b.put_u8(0);
}

/// Reads fields out of a message body.
struct Body<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Body<'a> {
    fn new(b: &'a [u8]) -> Self {
        Body { b, pos: 0 }
    }

    fn bad() -> Error {
        Error::new(code::PROTOCOL_VIOLATION, "invalid message format")
    }

    fn cstr(&mut self) -> Result<&'a str> {
        let rest = &self.b[self.pos.min(self.b.len())..];
        let n = rest.iter().position(|&c| c == 0).ok_or_else(Self::bad)?;
        self.pos += n + 1;
        std::str::from_utf8(&rest[..n]).map_err(|_| {
            Error::new(
                code::CHARACTER_NOT_IN_REPERTOIRE,
                "invalid byte sequence for encoding \"UTF8\"",
            )
        })
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.pos + n > self.b.len() {
            return Err(Self::bad());
        }
        let s = &self.b[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn i16(&mut self) -> Result<i16> {
        Ok(i16::from_be_bytes(self.bytes(2)?.try_into().unwrap()))
    }

    fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_be_bytes(self.bytes(4)?.try_into().unwrap()))
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }
}

// ── connection ──

/// Deregisters the backend when the connection ends, however it ends.
struct BackendGuard<'a> {
    engine: &'a Engine,
    pid: i32,
}

impl Drop for BackendGuard<'_> {
    fn drop(&mut self) {
        self.engine.unregister_backend(self.pid);
    }
}

fn handle(
    engine: Arc<Engine>,
    stream: TcpStream,
    peer: Option<SocketAddr>,
    opts: &ServerOptions,
) -> std::io::Result<()> {
    crate::stack::set_limit(THREAD_STACK - (2 << 20));
    stream.set_nodelay(true)?;
    let mut c = Conn {
        rd: BufReader::with_capacity(16 << 10, stream.try_clone()?),
        wr: stream.try_clone()?,
        out: BytesMut::with_capacity(8 << 10),
        partial: None,
        fatal: false,
    };

    // Startup: SSL / GSS negotiation and cancel requests come first.
    let params = loop {
        let len = c.read_i32()?;
        let body = c.read_body(len, 4)?;
        let mut b = Body::new(&body);
        let code = b.i32().map_err(io_err)?;
        match code {
            SSL_REQUEST | GSSENC_REQUEST => {
                c.wr.write_all(b"N")?;
            }
            CANCEL_REQUEST => {
                let (pid, secret) = (b.i32().map_err(io_err)?, b.i32().map_err(io_err)?);
                engine.cancel(pid, secret);
                return Ok(());
            }
            v if v >> 16 == 3 => {
                let mut params = Vec::new();
                loop {
                    let k = b.cstr().map_err(io_err)?;
                    if k.is_empty() {
                        break;
                    }
                    params.push((k.to_string(), b.cstr().map_err(io_err)?.to_string()));
                }
                // Newer 3.x minors and _pq_ options: tell the client what we speak.
                let unknown: Vec<&str> = params
                    .iter()
                    .map(|(k, _)| k.as_str())
                    .filter(|k| k.starts_with("_pq_."))
                    .collect();
                if v != PROTOCOL_3 || !unknown.is_empty() {
                    c.msg(b'v', |o| {
                        o.put_i32(PROTOCOL_3);
                        o.put_i32(unknown.len() as i32);
                        for u in &unknown {
                            cstr(o, u);
                        }
                    });
                }
                break params;
            }
            v => {
                let e = Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    format!(
                        "unsupported frontend protocol {}.{}: server supports 3.0 to 3.0",
                        v >> 16,
                        v & 0xffff
                    ),
                )
                .fatal();
                c.error(&e);
                return c.flush();
            }
        }
    };

    let (pid, secret, terminated) =
        engine.register_backend(stream.try_clone().ok(), peer.map(|p| p.ip().to_string()));
    let _guard = BackendGuard {
        engine: &engine,
        pid,
    };
    let get = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    let Some(user) = get("user").filter(|u| !u.is_empty()) else {
        c.error(
            &Error::new(
                code::INVALID_AUTHORIZATION_SPECIFICATION,
                "no PostgreSQL user name specified in startup packet",
            )
            .fatal(),
        );
        return c.flush();
    };
    let user = user.to_string();
    let database = get("database")
        .filter(|d| !d.is_empty())
        .unwrap_or(&user)
        .to_string();
    if get("replication").is_some_and(|r| r != "false" && r != "0" && r != "off") {
        c.error(
            &Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "replication connections are not supported",
            )
            .fatal(),
        );
        return c.flush();
    }

    if let Err(e) = authenticate(&mut c, &engine, &user, &database, opts.auth) {
        c.error(&e);
        return c.flush();
    }
    let app = get("application_name").unwrap_or("").to_string();
    let mut s = match engine.connect(pid, &database, &user, &app) {
        Ok(s) => s,
        Err(e) => {
            c.error(&e);
            return c.flush();
        }
    };
    if opts.auth == AuthMethod::ScramSha256 {
        s.state.system_user = Some(format!("scram-sha-256:{user}"));
    }

    // Settings from the startup packet: plain parameters and `options=-c name=value`.
    let mut settings: Vec<(String, String)> = Vec::new();
    for (k, v) in &params {
        match k.as_str() {
            "user" | "database" | "replication" => {}
            "options" => settings.extend(parse_options(v)),
            k if k.starts_with("_pq_.") => {}
            k => settings.push((k.to_string(), v.clone())),
        }
    }
    for (k, v) in settings {
        // roll's proxy routes on `endpoint`; it means nothing here.
        if k == "endpoint" {
            continue;
        }
        // Startup-packet values (PGC_S_CLIENT) are what RESET returns to.
        if let Err(e) = s.state.settings.borrow_mut().set_default(&k, &v) {
            c.error(&e.fatal());
            return c.flush();
        }
    }
    s.take_changed_settings();
    // Notifications for an idle session go straight to the client.
    let wr = c.wr.try_clone()?;
    s.attach_notifications(Arc::new(move |notes: &[crate::notify::Notification]| {
        let mut b = BytesMut::new();
        for n in notes {
            notification(&mut b, n);
        }
        (&wr).write_all(&b)
    }));

    c.auth(0, &[]);
    {
        let st = s.state.settings.borrow();
        for name in crate::session::settings::REPORTED {
            if st.exists(name) {
                let v = st.show_value(name);
                c.msg(b'S', |b| {
                    cstr(b, name);
                    cstr(b, &v);
                });
            }
        }
    }
    c.msg(b'K', |b| {
        b.put_i32(pid);
        b.put_i32(secret);
    });
    // Login event triggers, before the session is ready.
    if let Err(e) = s.run_login_triggers() {
        s.notices.push(e);
    }
    send_notices(&mut c, &mut s);
    ready_for_query(&mut c, &s)?;

    let r = message_loop(&mut c, &engine, &mut s, pid, &terminated);
    if terminated.load(Ordering::Relaxed) && !c.fatal {
        c.error(
            &Error::new(
                code::ADMIN_SHUTDOWN,
                "terminating connection due to administrator command",
            )
            .fatal(),
        );
        let _ = c.flush();
    }
    r
}

fn io_err(e: Error) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, e.message)
}

/// `-c name=value` / `--name=value` pairs from the startup `options` parameter.
fn parse_options(s: &str) -> Vec<(String, String)> {
    // Backslash escapes a space inside a value.
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            c if c.is_whitespace() => {
                if !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    let mut out = Vec::new();
    let mut it = words.into_iter();
    while let Some(w) = it.next() {
        let kv = if w == "-c" {
            it.next()
        } else {
            w.strip_prefix("--")
                .or_else(|| w.strip_prefix("-c"))
                .map(str::to_string)
        };
        if let Some((k, v)) = kv.as_deref().and_then(|kv| kv.split_once('=')) {
            out.push((k.replace('-', "_"), v.to_string()));
        }
    }
    out
}

fn authenticate(
    c: &mut Conn,
    engine: &Engine,
    user: &str,
    database: &str,
    method: AuthMethod,
) -> Result<()> {
    let cat = engine.auth_catalog(database)?;
    let role = cat.role_by_name(user).cloned();
    let failed = || {
        Error::new(
            code::INVALID_PASSWORD,
            format!("password authentication failed for user \"{user}\""),
        )
        .fatal()
    };
    match method {
        AuthMethod::Trust => {
            let Some(role) = role else {
                return Err(Error::new(
                    code::INVALID_AUTHORIZATION_SPECIFICATION,
                    format!("role \"{user}\" does not exist"),
                )
                .fatal());
            };
            if !role.login {
                return Err(Error::new(
                    code::INVALID_AUTHORIZATION_SPECIFICATION,
                    format!("role \"{user}\" is not permitted to log in"),
                )
                .fatal());
            }
            Ok(())
        }
        AuthMethod::ScramSha256 => {
            // Unknown roles and roles without a password still go through the exchange, with a
            // throwaway verifier, so the reply doesn't reveal which roles exist.
            let stored = role
                .as_ref()
                .and_then(|r| r.password.as_deref())
                .and_then(parse_verifier);
            let real = stored.is_some();
            let verifier = match stored {
                Some(v) => v,
                None => parse_verifier(&hash_password(&format!("{:x}", rand_u64())))
                    .ok_or_else(|| crate::error::internal("verifier"))?,
            };
            let mut mech = Vec::new();
            mech.extend_from_slice(b"SCRAM-SHA-256\0\0");
            c.auth(10, &mech);
            c.flush()?;
            let body = expect_password_message(c)?;
            let mut b = Body::new(&body);
            let mechanism = b.cstr()?;
            if mechanism != "SCRAM-SHA-256" {
                return Err(Error::new(
                    code::PROTOCOL_VIOLATION,
                    "client selected an invalid SASL authentication mechanism",
                )
                .fatal());
            }
            let n = b.i32()?;
            let first = if n < 0 {
                ""
            } else {
                std::str::from_utf8(b.bytes(n as usize)?).map_err(|_| Body::bad())?
            };
            let (scram, server_first) = ScramServer::start(verifier, first)
                .map_err(|m| Error::new(code::PROTOCOL_VIOLATION, m).fatal())?;
            c.auth(11, server_first.as_bytes());
            c.flush()?;
            let body = expect_password_message(c)?;
            let fin = std::str::from_utf8(&body).map_err(|_| Body::bad())?;
            let server_final = scram.finish(fin).map_err(|_| failed())?;
            // An expired password fails like a wrong one.
            let expired = role
                .as_ref()
                .and_then(|r| r.valid_until)
                .is_some_and(|t| t < crate::types::datetime::Clock::system().now);
            if !real || expired || !role.as_ref().is_some_and(|r| r.login) {
                return Err(match role {
                    Some(r) if real && !expired && !r.login => Error::new(
                        code::INVALID_AUTHORIZATION_SPECIFICATION,
                        format!("role \"{user}\" is not permitted to log in"),
                    )
                    .fatal(),
                    _ => failed(),
                });
            }
            c.auth(12, server_final.as_bytes());
            Ok(())
        }
    }
}

fn expect_password_message(c: &mut Conn) -> Result<Vec<u8>> {
    match c.read_message()? {
        Some((b'p', body)) => Ok(body),
        Some((t, _)) => Err(Error::new(
            code::PROTOCOL_VIOLATION,
            format!("expected SASL response, got message type {}", t),
        )
        .fatal()),
        None => Err(Error::new(
            code::PROTOCOL_VIOLATION,
            "connection closed during authentication",
        )
        .fatal()),
    }
}

fn rand_u64() -> u64 {
    let mut b = [0u8; 8];
    let _ = getrandom::fill(&mut b);
    u64::from_le_bytes(b)
}

// ── queries ──

fn activity_state(s: &Session) -> &'static str {
    match s.status() {
        TxnStatus::Idle => "idle",
        TxnStatus::InBlock => "idle in transaction",
        TxnStatus::Failed => "idle in transaction (aborted)",
    }
}

fn message_loop(
    c: &mut Conn,
    engine: &Engine,
    s: &mut Session,
    pid: i32,
    terminated: &AtomicBool,
) -> std::io::Result<()> {
    // After an error in extended protocol, messages are skipped until Sync.
    let mut skipping = false;
    loop {
        let Some((t, body)) = c.read_message()? else {
            return Ok(());
        };
        s.notifications_busy();
        if terminated.load(Ordering::Relaxed) {
            return Ok(());
        }
        if skipping && t != b'S' && t != b'X' {
            continue;
        }
        sync_client_encoding(s);
        // Query text arrives in the client's encoding.
        let body = match t {
            b'Q' | b'P' => match convert_cstrings(&body, 2) {
                Ok(b) => b,
                Err(e) => {
                    c.error(&e);
                    if t == b'Q' {
                        ready_for_query(c, s)?;
                    } else {
                        skipping = true;
                    }
                    continue;
                }
            },
            _ => body,
        };
        let r: Result<()> = match t {
            b'Q' => {
                let sql = match Body::new(&body).cstr() {
                    Ok(q) => q.to_string(),
                    Err(e) => {
                        c.error(&e);
                        ready_for_query(c, s)?;
                        continue;
                    }
                };
                let now = crate::types::datetime::Clock::system().now;
                engine.set_activity(pid, |a| {
                    a.state = "active".into();
                    a.query = sql.clone();
                    a.query_start = Some(now);
                });
                match guard(c, s, |c, s| Ok(simple_query(c, s, &sql))) {
                    Ok(r) => r?,
                    Err(e) => c.error(&e),
                }
                engine.set_activity(pid, |a| a.state = activity_state(s).into());
                // A FATAL error (the session terminating itself) ends the session.
                if c.fatal {
                    return c.flush();
                }
                ready_for_query(c, s)?;
                continue;
            }
            b'P' => guard(c, s, |c, s| parse_msg(c, s, &body)),
            b'B' => guard(c, s, |c, s| bind_msg(c, s, &body)),
            b'D' => guard(c, s, |c, s| describe_msg(c, s, &body)),
            b'E' => {
                engine.set_activity(pid, |a| a.state = "active".into());
                let r = guard(c, s, |c, s| execute_msg(c, s, &body));
                engine.set_activity(pid, |a| a.state = activity_state(s).into());
                r
            }
            b'C' => guard(c, s, |c, s| close_msg(c, s, &body)),
            b'S' => {
                skipping = false;
                if let Err(e) = guard(c, s, |_, s| s.sync()) {
                    send_notices(c, s);
                    c.error(&e);
                }
                send_notices(c, s);
                send_changed(c, s);
                ready_for_query(c, s)?;
                continue;
            }
            b'H' => {
                c.flush()?;
                continue;
            }
            b'X' => return Ok(()),
            // Copy messages outside COPY are ignored, as Postgres does.
            b'd' | b'c' | b'f' => continue,
            b'F' => {
                let now = crate::types::datetime::Clock::system().now;
                engine.set_activity(pid, |a| {
                    a.state = "fastpath function call".into();
                    a.query_start = Some(now);
                });
                let r = guard(c, s, |c, s| fastpath_msg(c, s, &body));
                send_notices(c, s);
                if let Err(e) = r {
                    c.error(&e);
                }
                engine.set_activity(pid, |a| a.state = activity_state(s).into());
                ready_for_query(c, s)?;
                continue;
            }
            t => {
                c.error(
                    &Error::new(
                        code::PROTOCOL_VIOLATION,
                        format!("invalid frontend message type {t}"),
                    )
                    .fatal(),
                );
                return c.flush();
            }
        };
        if let Err(e) = r {
            send_notices(c, s);
            c.error(&e);
            skipping = true;
            // Like elog's send_message_to_frontend, an error goes out at once (a pipelining
            // client may be waiting for it without having sent Sync or Flush).
            c.flush()?;
            if c.fatal {
                return Ok(());
            }
        }
        c.maybe_flush()?;
    }
}

/// Run one message's work so that an engine panic becomes an ERROR (XX000) for this
/// statement instead of taking the connection down: a half-written message is dropped and
/// the transaction fails as after any error.
fn guard<T>(
    c: &mut Conn,
    s: &mut Session,
    f: impl FnOnce(&mut Conn, &mut Session) -> Result<T>,
) -> Result<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&mut *c, &mut *s))) {
        Ok(r) => r,
        Err(payload) => {
            let what = payload
                .downcast_ref::<&str>()
                .map(|m| m.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown cause".into());
            c.drop_partial();
            s.recover_from_panic();
            send_notices(c, s);
            let e = crate::error::internal(format!("internal engine error: {what}"));
            Err(e)
        }
    }
}

/// ReadyForQuery, preceded by notifications received meanwhile (outside a transaction block);
/// then the session waits, and notifications go straight to the client.
fn ready_for_query(c: &mut Conn, s: &Session) -> std::io::Result<()> {
    if s.status() == TxnStatus::Idle {
        for n in s.take_notifications() {
            notification(&mut c.out, &n);
        }
    }
    c.ready(s.status())?;
    s.notifications_idle();
    Ok(())
}

/// NotificationResponse.
fn notification(b: &mut BytesMut, n: &crate::notify::Notification) {
    b.put_u8(b'A');
    let at = b.len();
    b.put_i32(0);
    b.put_i32(n.pid);
    cstr(b, &n.channel);
    cstr(b, &n.payload);
    let len = (b.len() - at) as i32;
    b[at..at + 4].copy_from_slice(&len.to_be_bytes());
}

fn send_notices(c: &mut Conn, s: &mut Session) {
    s.notices.extend(crate::types::tsearch::take_notices());
    for n in std::mem::take(&mut s.notices) {
        c.notice(&n);
    }
}

fn send_changed(c: &mut Conn, s: &mut Session) {
    for (k, v) in s.take_changed_settings() {
        c.parameter_status(&k, &v);
    }
}

fn row_description(c: &mut Conn, cols: &[OutCol], formats: &[i16]) {
    c.msg(b'T', |b| {
        b.put_i16(cols.len() as i16);
        for (i, col) in cols.iter().enumerate() {
            cstr(b, &col.name);
            b.put_u32(col.table);
            b.put_i16(col.attnum);
            b.put_u32(col.type_oid());
            b.put_i16(col.ty.typlen());
            b.put_i32(col.ty.typmod());
            b.put_i16(result_format(formats, i));
        }
    });
}

fn result_format(formats: &[i16], i: usize) -> i16 {
    match formats.len() {
        0 => 0,
        1 => formats[0],
        _ => formats.get(i).copied().unwrap_or(0),
    }
}

fn data_rows(c: &mut Conn, r: &QueryResult, formats: &[i16], fmt: &FormatCtx) -> Result<()> {
    let cols = r.cols.as_deref().unwrap_or(&[]);
    for row in &r.rows {
        let mut err = None;
        c.msg(b'D', |b| {
            b.put_i16(row.len() as i16);
            for (i, v) in row.iter().enumerate() {
                if v.is_null() {
                    b.put_i32(-1);
                    continue;
                }
                let ty = cols
                    .get(i)
                    .map(|c| c.ty.clone())
                    .unwrap_or(crate::types::DataType::Text);
                let at = b.len();
                b.put_i32(0);
                if result_format(formats, i) == 1 {
                    if let Err(e) = crate::types::binary::encode(v, &ty, fmt, b) {
                        err.get_or_insert(e);
                    }
                } else {
                    match to_client(&format_value(v, &ty, fmt)) {
                        Ok(t) => b.put_slice(&t),
                        Err(e) => {
                            err.get_or_insert(e);
                        }
                    }
                }
                let n = (b.len() - at - 4) as i32;
                b[at..at + 4].copy_from_slice(&n.to_be_bytes());
            }
        });
        if let Some(e) = err {
            return Err(e);
        }
        c.maybe_flush()?;
    }
    Ok(())
}

fn command_complete(c: &mut Conn, tag: &str) {
    c.msg(b'C', |b| cstr(b, tag));
}

fn simple_query(c: &mut Conn, s: &mut Session, sql: &str) -> std::io::Result<()> {
    let stmts = match s.parse(sql) {
        Ok(v) => v,
        Err(e) => {
            s.on_error();
            // The scanner's warnings came first.
            send_notices(c, s);
            c.error(&e.in_chars(sql));
            return Ok(());
        }
    };
    if stmts.is_empty() || stmts.iter().all(|p| p.stmt.is_none()) {
        c.msg(b'I', |_| {});
        return Ok(());
    }
    s.set_batch_len(stmts.len());
    let r = simple_batch(c, s, sql, &stmts);
    s.set_batch_len(0);
    r
}

fn simple_batch(
    c: &mut Conn,
    s: &mut Session,
    sql: &str,
    stmts: &[Arc<Prepared>],
) -> std::io::Result<()> {
    for (i, p) in stmts.iter().enumerate() {
        s.partial_rows = None;
        let mut out = s.execute(p);
        // Like Postgres, an implicit transaction commits before the last command tag goes out,
        // so deferred constraint failures replace it.
        if i + 1 == stmts.len()
            && matches!(out, Ok(Outcome::Done(_)))
            && let Err(e) = s.end_batch()
        {
            out = Err(e);
        }
        send_notices(c, s);
        let r = match out {
            Ok(o) => outcome(c, s, o, &[]),
            Err(e) => {
                // The rows a SELECT made before failing went out before the error.
                if let Some(rows) = s.partial_rows.take()
                    && let Some(cols) = &rows.cols
                {
                    row_description(c, cols, &[]);
                    let fmt = s.format_ctx();
                    let _ = data_rows(c, &rows, &[], &fmt);
                }
                Err(e)
            }
        };
        send_changed(c, s);
        if let Err(e) = r {
            let _ = s.end_batch();
            c.error(&e.in_chars(sql));
            return Ok(());
        }
    }
    if let Err(e) = s.end_batch() {
        c.error(&e);
    }
    send_notices(c, s);
    Ok(())
}

/// Send a statement's results: rows and the command tag, or run a COPY.
fn outcome(c: &mut Conn, s: &mut Session, o: Outcome, formats: &[i16]) -> Result<()> {
    match o {
        Outcome::Done(r) => {
            if let Some(cols) = &r.cols
                && formats.is_empty()
            {
                row_description(c, cols, &[]);
            }
            let fmt = s.format_ctx();
            data_rows(c, &r, formats, &fmt)?;
            command_complete(c, &r.tag);
            Ok(())
        }
        Outcome::CopyOut(out) => {
            let f = if out.binary { 1 } else { 0 };
            c.msg(b'H', |b| {
                b.put_i8(f);
                b.put_i16(out.ncols as i16);
                for _ in 0..out.ncols {
                    b.put_i16(f as i16);
                }
            });
            for d in &out.data {
                c.msg(b'd', |b| b.put_slice(d));
                c.maybe_flush()?;
            }
            c.msg(b'c', |_| {});
            for n in &out.trailing {
                c.notice(n);
            }
            command_complete(c, &out.tag);
            Ok(())
        }
        Outcome::CopyIn(cin) => {
            let f: i8 = if cin.format.binary { 1 } else { 0 };
            c.msg(b'G', |b| {
                b.put_i8(f);
                b.put_i16(cin.ncols as i16);
                for _ in 0..cin.ncols {
                    b.put_i16(f as i16);
                }
            });
            c.flush()?;
            let mut data = Vec::new();
            loop {
                match c.read_message()? {
                    Some((b'd', body)) => data.extend_from_slice(&body),
                    Some((b'c', _)) => break,
                    Some((b'f', body)) => {
                        let msg = Body::new(&body).cstr().unwrap_or("").to_string();
                        s.on_error();
                        return Err(Error::new(
                            code::QUERY_CANCELED,
                            format!("COPY from stdin failed: {msg}"),
                        ));
                    }
                    Some((b'H' | b'S', _)) => {}
                    Some((t, _)) => {
                        s.on_error();
                        return Err(Error::new(
                            code::PROTOCOL_VIOLATION,
                            format!("unexpected message type 0x{t:02X} during COPY from stdin"),
                        ));
                    }
                    None => {
                        return Err(Error::new(
                            code::CONNECTION_EXCEPTION,
                            "unexpected EOF on client connection with an open transaction",
                        ));
                    }
                }
            }
            let r = s.copy_in_finish(cin, &data);
            send_notices(c, s);
            command_complete(c, &r?.tag);
            Ok(())
        }
    }
}

fn parse_msg(c: &mut Conn, s: &mut Session, body: &[u8]) -> Result<()> {
    let mut b = Body::new(body);
    let name = b.cstr()?.to_string();
    let sql = b.cstr()?.to_string();
    let n = b.i16()?.max(0) as usize;
    let mut oids = Vec::with_capacity(n);
    for _ in 0..n {
        oids.push(b.i32()? as u32);
    }
    if name.is_empty() {
        s.close_statement("");
    }
    s.parse_message(&name, &sql, &oids)
        .map_err(|e| e.in_chars(&sql))?;
    send_notices(c, s);
    c.msg(b'1', |_| {});
    Ok(())
}

fn bind_msg(c: &mut Conn, s: &mut Session, body: &[u8]) -> Result<()> {
    let mut b = Body::new(body);
    let portal = b.cstr()?.to_string();
    let stmt = b.cstr()?.to_string();
    let nf = b.i16()?.max(0) as usize;
    let mut formats = Vec::with_capacity(nf);
    for _ in 0..nf {
        formats.push(b.i16()?);
    }
    let np = b.i16()?.max(0) as usize;
    let mut params = Vec::with_capacity(np);
    for _ in 0..np {
        let len = b.i32()?;
        params.push(if len < 0 {
            None
        } else {
            Some(b.bytes(len as usize)?.to_vec())
        });
    }
    let nr = b.i16()?.max(0) as usize;
    let mut result_formats = Vec::with_capacity(nr);
    for _ in 0..nr {
        result_formats.push(b.i16()?);
    }
    s.bind_message(&portal, &stmt, params, &formats, result_formats)?;
    send_notices(c, s);
    c.msg(b'2', |_| {});
    Ok(())
}

fn describe_msg(c: &mut Conn, s: &mut Session, body: &[u8]) -> Result<()> {
    let mut b = Body::new(body);
    let kind = b.u8()?;
    let name = b.cstr()?.to_string();
    match kind {
        b'S' => {
            let Some(p) = s.prepared.get(&name).cloned() else {
                return Err(Error::new(
                    code::INVALID_SQL_STATEMENT_NAME,
                    format!("prepared statement \"{name}\" does not exist"),
                ));
            };
            let cols = s.describe(&p)?;
            c.msg(b't', |o| {
                o.put_i16(p.param_types.len() as i16);
                for t in &p.param_types {
                    o.put_u32(t.oid());
                }
            });
            match cols {
                Some(cols) => row_description(c, &cols, &[]),
                None => c.msg(b'n', |_| {}),
            }
        }
        b'P' => {
            let Some(pt) = s.portals.get(&name) else {
                return Err(Error::new(
                    code::INVALID_CURSOR_NAME,
                    format!("portal \"{name}\" does not exist"),
                ));
            };
            match pt.cols.clone() {
                Some(cols) => {
                    let f = pt.result_formats.clone();
                    row_description(c, &cols, &f);
                }
                None => c.msg(b'n', |_| {}),
            }
        }
        _ => {
            return Err(Error::new(
                code::PROTOCOL_VIOLATION,
                format!("invalid DESCRIBE message subtype {kind}"),
            ));
        }
    }
    Ok(())
}

fn execute_msg(c: &mut Conn, s: &mut Session, body: &[u8]) -> Result<()> {
    let mut b = Body::new(body);
    let portal = b.cstr()?.to_string();
    let max_rows = b.i32()?.max(0) as usize;
    let (formats, sql, empty) = match s.portals.get(&portal) {
        Some(p) => (
            p.result_formats.clone(),
            p.stmt.sql.clone(),
            p.stmt.stmt.is_none(),
        ),
        None => (Vec::new(), String::new(), false),
    };
    if empty {
        c.msg(b'I', |_| {});
        return Ok(());
    }
    let r = s.execute_portal(&portal, max_rows);
    if r.is_ok() {
        s.note_executed();
    }
    send_notices(c, s);
    let (o, more) = r.map_err(|e| e.in_chars(&sql))?;
    match o {
        Outcome::Done(r) if more => {
            let fmt = s.format_ctx();
            data_rows(c, &r, &formats, &fmt)?;
            c.msg(b's', |_| {});
        }
        // Rows only: Execute doesn't send a RowDescription.
        Outcome::Done(r) => {
            let fmt = s.format_ctx();
            data_rows(c, &r, &formats, &fmt)?;
            command_complete(c, &r.tag);
        }
        o => outcome(c, s, o, &formats)?,
    }
    send_changed(c, s);
    Ok(())
}

/// FunctionCall: a function by oid, its arguments, and the result's format.
fn fastpath_msg(c: &mut Conn, s: &mut Session, body: &[u8]) -> Result<()> {
    let mut b = Body::new(body);
    let f = s.fastpath_function(b.i32()? as u32)?;
    let (formats, args, rformat) = fastpath_args(&mut b).inspect_err(|_| s.on_error())?;
    let v = s.fastpath_call(&f, &formats, args, rformat)?;
    send_notices(c, s);
    c.msg(b'V', |o| match &v {
        Some(bytes) => {
            o.put_i32(bytes.len() as i32);
            o.put_slice(bytes);
        }
        None => o.put_i32(-1),
    });
    Ok(())
}

/// parse_fcall_arguments: the argument formats, the arguments, and the result format.
#[allow(clippy::type_complexity)]
fn fastpath_args(b: &mut Body) -> Result<(Vec<i16>, Vec<Option<Vec<u8>>>, i16)> {
    let nf = b.i16()?.max(0) as usize;
    let mut formats = Vec::with_capacity(nf);
    for _ in 0..nf {
        formats.push(b.i16()?);
    }
    let nargs = b.i16()?.max(0);
    let mut args = Vec::with_capacity(nargs as usize);
    for _ in 0..nargs {
        let len = b.i32()?;
        if len < -1 {
            return Err(Error::new(
                code::PROTOCOL_VIOLATION,
                format!("invalid argument size {len} in function call message"),
            ));
        }
        args.push(if len == -1 {
            None
        } else {
            Some(b.bytes(len as usize)?.to_vec())
        });
    }
    let rformat = b.i16()?;
    if b.pos != b.b.len() {
        return Err(Body::bad());
    }
    Ok((formats, args, rformat))
}

fn close_msg(c: &mut Conn, s: &mut Session, body: &[u8]) -> Result<()> {
    let mut b = Body::new(body);
    let kind = b.u8()?;
    let name = b.cstr()?.to_string();
    match kind {
        b'S' => s.close_statement(&name),
        b'P' => s.close_portal(&name),
        _ => {
            return Err(Error::new(
                code::PROTOCOL_VIOLATION,
                format!("invalid CLOSE message subtype {kind}"),
            ));
        }
    }
    c.msg(b'3', |_| {});
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_options;

    #[test]
    fn startup_options() {
        assert_eq!(
            parse_options(
                "-c search_path=app,public --statement-timeout=5s -cwork_mem=4MB -c a=b\\ c"
            ),
            vec![
                ("search_path".into(), "app,public".into()),
                ("statement_timeout".into(), "5s".into()),
                ("work_mem".into(), "4MB".into()),
                ("a".into(), "b c".into()),
            ]
        );
    }
}

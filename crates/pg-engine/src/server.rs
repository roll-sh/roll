//! The Postgres wire protocol (v3): startup, SCRAM-SHA-256 authentication, simple and
//! extended query, COPY, cancel requests. One thread per connection: sessions are not
//! `Send`, and a thread with a big stack gives deep queries room before
//! "stack depth limit exceeded".

use crate::auth::{ScramServer, hash_password, md5_response, parse_verifier, verify_plain};
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
/// A connection thread's stack. An unoptimized build's frames are many times a release
/// build's (a PL/pgSQL recursion level takes ~250 kB of it), so it gets more, to recurse
/// as deep as Postgres does at max_stack_depth's 2 MB (subxid-overflow's 100 levels with
/// exception blocks); untouched pages cost nothing.
#[cfg(not(debug_assertions))]
const THREAD_STACK: usize = 16 << 20;
#[cfg(debug_assertions)]
const THREAD_STACK: usize = 64 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthMethod {
    /// Anyone who names an existing role gets in (local development).
    Trust,
    /// SCRAM-SHA-256 against the role's stored verifier (pg_hba's `scram-sha-256`).
    ScramSha256,
    /// pg_hba's `md5`: an MD5 challenge for a role with an MD5 password, SCRAM for one with a
    /// SCRAM secret.
    Md5,
    /// pg_hba's `password`: the password in clear, checked against either kind.
    Password,
}

impl AuthMethod {
    /// hba_authname: what system_user shows before the identity.
    fn name(self) -> &'static str {
        match self {
            AuthMethod::Trust => "trust",
            AuthMethod::ScramSha256 => "scram-sha-256",
            AuthMethod::Md5 => "md5",
            AuthMethod::Password => "password",
        }
    }

    /// The `--auth` option's values (pg_hba method names).
    pub fn parse(s: &str) -> Option<AuthMethod> {
        Some(match s {
            "trust" => AuthMethod::Trust,
            "scram-sha-256" => AuthMethod::ScramSha256,
            "md5" => AuthMethod::Md5,
            "password" => AuthMethod::Password,
            _ => return None,
        })
    }
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
    if let Ok(a) = listener.local_addr() {
        engine.note_listen_addr(a);
    }
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

/// What a connection's thread runs once it has waited `IDLE_TRIM` for its client
/// (`set_idle_hook`).
static IDLE_HOOK: std::sync::OnceLock<fn()> = std::sync::OnceLock::new();

/// How long a connection waits for its client before running the idle hook.
const IDLE_TRIM: std::time::Duration = std::time::Duration::from_secs(2);

/// Run `f` on a connection's thread once the connection has been idle for a while (once per
/// idle period): the server binary returns the thread's free allocator memory to the
/// system there, so idle sessions keep little. Set once, before serving.
pub fn set_idle_hook(f: fn()) {
    let _ = IDLE_HOOK.set(f);
}

/// A connection's reading side: a read that waits past `IDLE_TRIM` runs the idle hook (once
/// until the client sends something again) and goes on waiting; one waiting past `deadline`
/// (an idle timeout) fails with `IdleTimeout`.
struct IdleRead {
    s: TcpStream,
    armed: bool,
    deadline: Option<std::time::Instant>,
}

/// The error a read past `IdleRead::deadline` fails with.
#[derive(Debug)]
struct IdleTimeout;

impl std::fmt::Display for IdleTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("idle timeout")
    }
}

impl std::error::Error for IdleTimeout {}

impl IdleRead {
    fn new(s: TcpStream) -> std::io::Result<IdleRead> {
        if IDLE_HOOK.get().is_some() {
            s.set_read_timeout(Some(IDLE_TRIM))?;
        }
        Ok(IdleRead { s, armed: true, deadline: None })
    }

    /// The socket's read timeout: the idle hook's period, and no later than the deadline.
    fn wait_for(&self) -> Option<std::time::Duration> {
        let hook = IDLE_HOOK.get().map(|_| IDLE_TRIM);
        let left = self.deadline.map(|d| {
            d.saturating_duration_since(std::time::Instant::now())
                .max(std::time::Duration::from_millis(1))
        });
        match (hook, left) {
            (Some(h), Some(l)) => Some(h.min(l)),
            (h, l) => h.or(l),
        }
    }
}

impl Read for IdleRead {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.deadline.is_some() {
            self.s.set_read_timeout(self.wait_for())?;
        }
        loop {
            match self.s.read(buf) {
                Ok(n) => {
                    self.armed = true;
                    return Ok(n);
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    if self.deadline.is_some_and(|d| std::time::Instant::now() >= d) {
                        return Err(std::io::Error::other(IdleTimeout));
                    }
                    if std::mem::take(&mut self.armed)
                        && let Some(h) = IDLE_HOOK.get()
                    {
                        h();
                    }
                    if self.deadline.is_some() {
                        self.s.set_read_timeout(self.wait_for())?;
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }
}

// ── framing ──

struct Conn {
    rd: BufReader<IdleRead>,
    wr: TcpStream,
    out: BytesMut,
    /// Where the message being written starts (to drop it if a panic cuts it short).
    partial: Option<usize>,
    /// A FATAL error went out: the session ends.
    fatal: bool,
    /// pg_terminate_backend was called on this session.
    terminated: Option<Arc<AtomicBool>>,
    /// The socket, for what goes out while a statement runs (`WireSink`).
    live: std::rc::Rc<TcpStream>,
    /// ReadyForQuery went out and no message came since: the session is idle, and its idle
    /// timeouts run (only then: not between the messages of an extended query).
    at_ready: bool,
    /// Errors and warnings sent, for the server log (kept only while it's written).
    sent: Vec<Error>,
    /// A walsender connection (`replication=...` in the startup packet).
    repl: Option<crate::repl::walsender::Mode>,
    /// The snapshot its last CREATE_REPLICATION_SLOT exported.
    exported: Option<crate::repl::walsender::Exported>,
}

impl crate::repl::walsender::Wire for Conn {
    fn put(&mut self, t: u8, body: &[u8]) {
        self.msg(t, |b| b.put_slice(body));
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Conn::flush(self)
    }

    fn poll(&mut self, wait: std::time::Duration) -> std::io::Result<Option<(u8, Vec<u8>)>> {
        let eof = || std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "unexpected EOF on standby connection");
        if self.rd.buffer().is_empty() {
            let sock = &self.rd.get_ref().s;
            let prev = sock.read_timeout()?;
            sock.set_read_timeout(Some(wait.max(std::time::Duration::from_millis(1))))?;
            let mut b = [0u8; 1];
            let r = sock.peek(&mut b);
            sock.set_read_timeout(prev)?;
            match r {
                Ok(0) => return Err(eof()),
                Ok(_) => {}
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                    return Ok(None);
                }
                Err(e) => return Err(e),
            }
        }
        self.read_message()?.map(Some).ok_or_else(eof)
    }
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
        self.read_message_by(None)
    }

    /// `read_message`, its first byte awaited no later than `deadline` (an idle timeout:
    /// the read fails with `IdleTimeout`).
    fn read_message_by(
        &mut self,
        deadline: Option<std::time::Instant>,
    ) -> std::io::Result<Option<(u8, Vec<u8>)>> {
        let mut t = [0u8; 1];
        self.rd.get_mut().deadline = deadline;
        let first = self.rd.read(&mut t);
        if deadline.is_some() {
            let r = self.rd.get_mut();
            r.deadline = None;
            r.s.set_read_timeout(IDLE_HOOK.get().map(|_| IDLE_TRIM))?;
        }
        if first? == 0 {
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
        // (And what a statement cut short had parked or sent into `UNFLUSHED`.)
        UNFLUSHED.with(|u| u.borrow_mut().clear());
        if let Some(at) = self.partial.take()
            && at <= self.out.len()
        {
            self.out.truncate(at);
        }
    }

    /// Run `f` (a statement) with what's buffered so far parked where a notice sent while
    /// it runs goes out after it (`UNFLUSHED`); what's left of it comes back to the buffer.
    /// Meanwhile each notice goes out as it is raised (`exec::streaming_notices`).
    fn unflushed<T>(&mut self, f: impl FnOnce() -> T) -> T {
        UNFLUSHED.with(|u| *u.borrow_mut() = std::mem::take(&mut self.out));
        let r = crate::exec::streaming_notices(f);
        let mut parked = UNFLUSHED.with(|u| std::mem::take(&mut *u.borrow_mut()));
        parked.extend_from_slice(&self.out);
        self.out = parked;
        r
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
        // A statement interrupted by pg_terminate_backend ends the session (ProcessInterrupts:
        // ProcDiePending wins over QueryCancelPending).
        // (Or COPY FROM STDIN finding its input shut down by the termination.)
        if (e.code == code::QUERY_CANCELED || e.code == code::CONNECTION_EXCEPTION)
            && self.terminated.as_ref().is_some_and(|t| t.load(Ordering::Relaxed))
        {
            let mut fatal = Error::new(
                code::ADMIN_SHUTDOWN,
                "terminating connection due to administrator command",
            )
            .fatal();
            // (Raised where the statement was: its error context callbacks' lines, such as a
            // lock wait's "while updating tuple ...", are the FATAL's.)
            fatal.context = e.context.clone();
            return self.error(&fatal);
        }
        if e.severity == crate::error::Severity::Fatal {
            self.fatal = true;
        }
        if crate::log::active() {
            self.sent.push(e.clone());
        }
        self.msg(b'E', |b| error_fields(b, e, e.severity.as_str()));
    }

    fn notice(&mut self, e: &Error) {
        if e.severity == crate::error::Severity::Warning && crate::log::active() {
            self.sent.push(e.clone());
        }
        self.msg(b'N', |b| error_fields(b, e, e.severity.as_str()));
    }

    /// The errors and warnings sent since the last call go to the server log (the session's,
    /// or before there is one, the connection's backend), with `statement`.
    fn log_sent(&mut self, s: Option<&Session>, pid: i32, statement: Option<&str>) {
        for e in std::mem::take(&mut self.sent) {
            match s {
                Some(s) => s.log_error(&e, statement),
                None => {
                    let f = crate::log::Format::postmaster();
                    let who = crate::log::Who { pid, session: None, backend_type: "client backend" };
                    crate::log::write(&f, &who, &crate::log::Message::error(&e, None));
                }
            }
        }
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
    /// Messages buffered for the client while a statement runs (`Conn::unflushed`).
    static UNFLUSHED: std::cell::RefCell<BytesMut> = std::cell::RefCell::new(BytesMut::new());
    /// The session's client_encoding and its database's encoding (sessions run on threads
    /// of their own): text goes out in the first and query text comes in in it.
    static CLIENT_ENC: std::cell::Cell<(i32, i32)> =
        const { std::cell::Cell::new((crate::types::mbconv::UTF8, crate::types::mbconv::UTF8)) };
}

/// Take up the session's client_encoding for the messages that follow.
fn sync_client_encoding(s: &Session) {
    let st = s.state.settings.borrow();
    let id = |name| {
        st.get_str(name)
            .and_then(crate::types::encnames::encoding_id)
            .unwrap_or(crate::types::mbconv::UTF8)
    };
    let e = (id("client_encoding"), id("server_encoding"));
    CLIENT_ENC.with(|c| c.set(e));
}

fn client_enc() -> (i32, i32) {
    CLIENT_ENC.with(|c| c.get())
}

fn plain_utf8() -> bool {
    client_enc() == (crate::types::mbconv::UTF8, crate::types::mbconv::UTF8)
}

/// Server text in the client's encoding (pg_server_to_client).
fn to_client(s: &str) -> Result<std::borrow::Cow<'_, [u8]>> {
    let (client, server) = client_enc();
    crate::types::mbconv::server::to_client(s, client, server)
}

/// Client bytes as server text (pg_client_to_server).
fn from_client(b: &[u8]) -> Result<Vec<u8>> {
    if plain_utf8() {
        return Ok(b.to_vec());
    }
    let (client, server) = client_enc();
    Ok(crate::types::mbconv::server::from_client(b, client, server)?.into_bytes())
}

/// A message body whose first `n` NUL-terminated strings are converted from the client's
/// encoding (the rest, binary, kept).
fn convert_cstrings(body: &[u8], n: usize) -> Result<Vec<u8>> {
    if plain_utf8() {
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
    // FILE / LINE / ROUTINE (error_location).
    let (file, line, routine) = crate::error_location::location(e);
    for (t, v) in [(b'F', file), (b'L', line.as_str()), (b'R', routine)] {
        b.put_u8(t);
        cstr(b, v);
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
        // pq_getmsgstring
        let n = rest
            .iter()
            .position(|&c| c == 0)
            .ok_or_else(|| Error::new(code::PROTOCOL_VIOLATION, "invalid string in message"))?;
        self.pos += n + 1;
        std::str::from_utf8(&rest[..n]).map_err(|_| {
            Error::new(
                code::CHARACTER_NOT_IN_REPERTOIRE,
                "invalid byte sequence for encoding \"UTF8\"",
            )
        })
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        // pq_getmsgbytes
        if self.pos + n > self.b.len() {
            return Err(Error::new(
                code::PROTOCOL_VIOLATION,
                "insufficient data left in message",
            ));
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
        rd: BufReader::with_capacity(16 << 10, IdleRead::new(stream.try_clone()?)?),
        wr: stream.try_clone()?,
        out: BytesMut::with_capacity(8 << 10),
        partial: None,
        fatal: false,
        terminated: None,
        live: std::rc::Rc::new(stream.try_clone()?),
        at_ready: false,
        sent: Vec::new(),
        repl: None,
        exported: None,
    };
    // Notices a statement raises before it waits for a lock go out then, as Postgres
    // flushes each notice: after what the connection had buffered (`Conn::unflushed`).
    let mut live = stream.try_clone()?;
    crate::exec::set_notice_sink(Some(Box::new(move |ns: &[Error]| {
        let mut b = UNFLUSHED.with(|u| std::mem::take(&mut *u.borrow_mut()));
        for e in ns {
            b.put_u8(b'N');
            let at = b.len();
            b.put_i32(0);
            error_fields(&mut b, e, e.severity.as_str());
            let len = (b.len() - at) as i32;
            b[at..at + 4].copy_from_slice(&len.to_be_bytes());
        }
        let _ = live.write_all(&b);
    })));
    struct NoSink;
    impl Drop for NoSink {
        fn drop(&mut self) {
            crate::exec::set_notice_sink(None);
        }
    }
    let _no_sink = NoSink;

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

    // log_connections' receipt (the files' and the command line's value: the session's
    // settings aren't there yet).
    let lc = engine
        .command_line_value("log_connections")
        .map(str::to_string)
        .or_else(|| engine.server_conf().loaded().values.get("log_connections").map(|v| v.value.clone()))
        .unwrap_or_default();
    let (pid, secret, terminated) =
        engine.register_backend(stream.try_clone().ok(), peer);
    let _guard = BackendGuard {
        engine: &engine,
        pid,
    };
    c.terminated = Some(terminated.clone());
    if conn_logging(&lc, "receipt") && crate::log::active() {
        let (host, port) = peer.map_or(("[local]".to_string(), String::new()), |p| (p.ip().to_canonical().to_string(), p.port().to_string()));
        let who = crate::log::Who { pid, session: None, backend_type: "client backend" };
        let files = engine.server_conf().loaded();
        let mut f = crate::log::Format::postmaster();
        if let Some(v) = files.values.get("log_line_prefix") {
            f.prefix = v.value.clone();
        }
        crate::log::write(&f, &who, &crate::log::Message::log(&format!("connection received: host={host} port={port}")));
    }
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
    // ProcessStartupPacket: the database defaults to the user name; both are cut to
    // NAMEDATALEN - 1 bytes, so an overlength name finds the object its CREATE truncated.
    // (A byte cut, not pg_mbcliplen: a name cut inside a character matches nothing.)
    let name = |s: &str| {
        let b = s.as_bytes();
        String::from_utf8_lossy(&b[..b.len().min(63)]).into_owned()
    };
    let mut database = name(get("database").filter(|d| !d.is_empty()).unwrap_or(user));
    let user = name(user);
    // A walsender (replication=true: physical, without a database; =database: logical).
    if let Some(r) = get("replication") {
        match crate::repl::walsender::mode_of(r) {
            Some(m) => c.repl = Some(m),
            None if crate::types::text::parse_bool(r) == Ok(false) => {}
            None => {
                c.error(
                    &Error::new(
                        code::INVALID_PARAMETER_VALUE,
                        format!("invalid value for parameter \"replication\": \"{r}\""),
                    )
                    .hint("Valid values are: \"false\", 0, \"true\", 1, \"database\".")
                    .fatal(),
                );
                return c.flush();
            }
        }
        if c.repl == Some(crate::repl::walsender::Mode::Physical) {
            database = "postgres".into();
        }
    }

    let method = match hba_method(&engine, peer, &user, &database, opts.auth) {
        Ok(m) => m,
        Err(e) => {
            c.error(&e);
            return c.flush();
        }
    };
    if let Err(e) = authenticate(&mut c, &engine, &user, &database, method) {
        auth_delay(&engine);
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
    if method != AuthMethod::Trust {
        s.state.system_user = Some(format!("{}:{user}", method.name()));
    }
    if c.repl.is_some()
        && let Err(e) = crate::repl::walsender::check_role(&s.state.db.catalog(), &user)
    {
        c.error(&e);
        return c.flush();
    }

    // Settings from the startup packet: plain parameters and `options=-c name=value`.
    // (process_startup_options: the `options` switches first, then the other parameters,
    // which win over them.)
    let mut settings: Vec<(String, String)> = Vec::new();
    crate::session::settings::take_name_notices();
    for (k, v) in &params {
        if k == "options" {
            settings.extend(parse_options(v));
        }
    }
    for (k, v) in &params {
        match k.as_str() {
            "user" | "database" | "replication" | "options" => {}
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
        if let Err(e) = s.state.settings.borrow_mut().set_client_default(&k, &v) {
            c.error(&e.fatal());
            return c.flush();
        }
    }
    s.state.peer.set(peer);
    s.preload_libraries();
    if conn_logging(&s.state.settings.borrow().get_raw("log_connections").unwrap_or_default(), "authorization") {
        let app = s.state.settings.borrow().show_value("application_name");
        let line = format!("connection authorized: user={user} database={database} application_name={app}");
        s.log_message(&line, None);
        s.notices.clear();
    }
    s.take_changed_settings();
    // (process_startup_options' NOTICEs: an application_name truncated.)
    let startup_notices = crate::session::settings::take_name_notices();
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
    for e in &startup_notices {
        c.notice(e);
    }
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

    let started = std::time::Instant::now();
    let r = message_loop(&mut c, &engine, &mut s, pid, &terminated);
    c.log_sent(Some(&s), pid, None);
    if s.state.settings.borrow().bool("log_disconnections") {
        let t = started.elapsed();
        let (h, m, sec, ms) = (t.as_secs() / 3600, t.as_secs() / 60 % 60, t.as_secs() % 60, t.subsec_millis());
        let (host, port) = peer.map_or(("[local]".to_string(), String::new()), |p| (p.ip().to_canonical().to_string(), p.port().to_string()));
        let line = format!(
            "disconnection: session time: {h}:{m:02}:{sec:02}.{ms:03} user={user} database={database} host={host} port={port}"
        );
        s.log_message(&line, None);
    }
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

/// Whether log_connections (PostgreSQL 18's list: receipt, authentication, authorization,
/// setup_durations; on / all) covers `aspect`.
fn conn_logging(v: &str, aspect: &str) -> bool {
    let v = v.trim().to_ascii_lowercase();
    match v.as_str() {
        "on" | "true" | "yes" | "1" => aspect != "setup_durations",
        "all" => true,
        _ => v.split(',').any(|a| a.trim() == aspect),
    }
}

/// check_hba: the method pg_hba.conf's first matching rule names (without rules, the
/// server's `--auth` method for everyone); `reject` and no matching rule end the connection.
fn hba_method(
    engine: &Engine,
    peer: Option<SocketAddr>,
    user: &str,
    database: &str,
    default: AuthMethod,
) -> Result<AuthMethod> {
    let Some(hba) = engine.server_conf().hba() else {
        if let Some(p) = peer {
            crate::hba::take_local(p);
        }
        return Ok(default);
    };
    let local = peer.is_some_and(crate::hba::take_local);
    let host = match peer {
        Some(p) if !local => p.ip().to_canonical().to_string(),
        _ => "[local]".to_string(),
    };
    let cat = engine.auth_catalog(database).ok();
    let member = |role: &str| {
        cat.as_ref().is_some_and(|c| match (c.role_by_name(user), c.role_by_name(role)) {
            (Some(u), Some(r)) => c.is_member_of_role(u.oid, r.oid, 0),
            _ => false,
        })
    };
    let refused = |what: &str| {
        Error::new(
            code::INVALID_AUTHORIZATION_SPECIFICATION,
            format!("{what} for host \"{host}\", user \"{user}\", database \"{database}\", no encryption"),
        )
        .fatal()
    };
    let Some(rule) = hba.choose(peer.map(|p| p.ip().to_canonical()), local, database, user, &member) else {
        return Err(refused("no pg_hba.conf entry"));
    };
    Ok(match rule.method.as_str() {
        "trust" => AuthMethod::Trust,
        "scram-sha-256" => AuthMethod::ScramSha256,
        "md5" => AuthMethod::Md5,
        "password" => AuthMethod::Password,
        "reject" => return Err(refused("pg_hba.conf rejects connection")),
        m => {
            let name = match m {
                "ident" => "Ident",
                "peer" => "Peer",
                "gss" => "GSSAPI",
                "sspi" => "SSPI",
                "pam" => "PAM",
                "ldap" => "LDAP",
                "radius" => "RADIUS",
                "cert" => "certificate",
                _ => "OAuth bearer",
            };
            return Err(Error::new(
                code::INVALID_AUTHORIZATION_SPECIFICATION,
                format!("{name} authentication failed for user \"{user}\""),
            )
            .fatal());
        }
    })
}

/// auth_delay (preloaded): a failed authentication waits auth_delay.milliseconds before the
/// error goes out.
fn auth_delay(engine: &Engine) {
    let conf = engine.server_conf();
    let preloaded = engine.options().shared_preload_libraries.iter().any(|l| l == "auth_delay");
    let ms = conf
        .loaded()
        .values
        .get("auth_delay.milliseconds")
        .and_then(|v| v.value.parse::<u64>().ok())
        .or_else(|| engine.command_line_value("auth_delay.milliseconds").and_then(|v| v.parse().ok()))
        .unwrap_or(0);
    if preloaded && ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
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
        AuthMethod::Password | AuthMethod::Md5 if role.is_some() || method == AuthMethod::Password => {
            let shadow = role.as_ref().and_then(|r| r.password.clone());
            let md5_stored = shadow
                .as_deref()
                .is_some_and(|p| crate::auth::password_type(p) != crate::auth::PasswordType::Scram);
            let ok = if method == AuthMethod::Password {
                // AuthenticationCleartextPassword
                c.auth(3, &[]);
                c.flush()?;
                let body = expect_password_message(c)?;
                let given = Body::new(&body).cstr()?.to_string();
                shadow.as_deref().is_some_and(|sh| verify_plain(user, sh, &given))
            } else if md5_stored {
                // AuthenticationMD5Password, with a random salt.
                let salt = (rand_u64() as u32).to_be_bytes();
                c.auth(5, &salt);
                c.flush()?;
                let body = expect_password_message(c)?;
                let given = Body::new(&body).cstr()?.to_string();
                shadow
                    .as_deref()
                    .and_then(|sh| md5_response(sh, user, &salt))
                    .is_some_and(|want| want == given)
            } else {
                // A SCRAM secret under md5: the SCRAM exchange (CheckPWChallengeAuth).
                return authenticate(c, engine, user, database, AuthMethod::ScramSha256);
            };
            check_login(role.as_ref(), user, ok, failed)
        }
        // (Password never gets here: its arm above takes every role.)
        AuthMethod::ScramSha256 | AuthMethod::Md5 | AuthMethod::Password => {
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

/// After a password check: an expired password fails like a wrong one; a role without LOGIN
/// can't log in.
fn check_login(
    role: Option<&crate::catalog::Role>,
    user: &str,
    ok: bool,
    failed: impl Fn() -> Error,
) -> Result<()> {
    let expired = role
        .and_then(|r| r.valid_until)
        .is_some_and(|t| t < crate::types::datetime::Clock::system().now);
    match role {
        Some(r) if ok && !expired && !r.login => Err(Error::new(
            code::INVALID_AUTHORIZATION_SPECIFICATION,
            format!("role \"{user}\" is not permitted to log in"),
        )
        .fatal()),
        Some(_) if ok && !expired => Ok(()),
        _ => Err(failed()),
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

/// pg_stat_activity after a statement: the backend's state, and its application_name as
/// the statement left it (SET application_name reports itself: pgstat_report_appname).
fn statement_done_activity(engine: &Engine, pid: i32, s: &Session) {
    let app = s.state.settings.borrow().show("application_name").ok();
    let idle = s.status() == TxnStatus::Idle;
    let (xid, xmin) = s.activity_xids();
    let now = crate::types::datetime::Clock::system().now;
    engine.set_activity(pid, |a| {
        let state = activity_state(s);
        if a.state != state {
            a.state = state.into();
            a.state_change = Some(now);
        }
        if idle {
            a.xact_start = None;
        }
        a.backend_xid = xid;
        a.backend_xmin = xmin;
        if let Some(app) = app {
            a.application_name = app;
        }
    });
}

/// pg_stat_activity as a statement (or a fastpath call) starts: `state`, its transaction's
/// start (now, outside a transaction block), and the xmin a snapshot taken now has.
fn start_activity(engine: &Engine, pid: i32, s: &Session, state: &str, sql: Option<String>) {
    let now = crate::types::datetime::Clock::system().now;
    let xmin = s.state.db.xids().current_xmin();
    engine.set_activity(pid, |a| {
        if a.state != state {
            a.state = state.into();
            a.state_change = Some(now);
        }
        if let Some(q) = sql {
            a.query = q;
        }
        a.query_start = Some(now);
        a.xact_start.get_or_insert(now);
        a.backend_xmin.get_or_insert(xmin);
    });
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
    // The query of the last Parse, for what the server log says of errors after it.
    let mut last_stmt: Option<String> = None;
    loop {
        c.log_sent(Some(s), pid, last_stmt.as_deref());
        let idle = if std::mem::take(&mut c.at_ready) { s.idle_timeout() } else { None };
        let msg = match c.read_message_by(idle.as_ref().map(|i| i.0)) {
            Ok(m) => m,
            Err(e) if e.get_ref().is_some_and(|e| e.is::<IdleTimeout>()) => {
                // The timeout's FATAL, then the session ends (its transaction rolls back).
                c.error(&idle.unwrap().1);
                return c.flush();
            }
            Err(e) => return Err(e),
        };
        let Some((t, body)) = msg else {
            return Ok(());
        };
        s.notifications_busy();
        // A cancel that came while the session waited for this message cancels nothing
        // (ProcessInterrupts ignores QueryCancelPending while DoingCommandRead). (Cleared
        // before the check: a termination sets `terminated` before the cancel flag.)
        s.cancel.store(false, Ordering::Relaxed);
        if terminated.load(Ordering::Relaxed) {
            return Ok(());
        }
        if skipping && t != b'S' && t != b'X' {
            continue;
        }
        sync_client_encoding(s);
        s.poll_config();
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
                start_activity(engine, pid, s, "active", Some(sql.clone()));
                if let Some(mode) = c.repl
                    && (crate::repl::cmd::is_command(&sql) || mode == crate::repl::walsender::Mode::Physical)
                {
                    let r = guard(c, s, |c, s| {
                        if !crate::repl::cmd::is_command(&sql) {
                            return Err(Error::new(
                                code::FEATURE_NOT_SUPPORTED,
                                "cannot execute SQL commands in WAL sender for physical replication",
                            ));
                        }
                        let cmd = crate::repl::cmd::parse(&sql)?;
                        let mut exported = c.exported.take();
                        let r = crate::repl::walsender::exec(c, s, mode, cmd, &mut exported);
                        c.exported = exported;
                        r
                    });
                    if let Err(e) = r {
                        s.on_error();
                        send_notices(c, s);
                        c.error(&e);
                    }
                } else {
                    match guard(c, s, |c, s| Ok(simple_query(c, s, &sql))) {
                        Ok(r) => r?,
                        Err(e) => c.error(&e),
                    }
                }
                statement_done_activity(engine, pid, s);
                c.log_sent(Some(s), pid, Some(&sql));
                // A FATAL error (the session terminating itself) ends the session.
                if c.fatal {
                    return c.flush();
                }
                ready_for_query(c, s)?;
                continue;
            }
            b'P' => {
                let mut b = Body::new(&body);
                last_stmt = b.cstr().ok().and_then(|_| b.cstr().ok()).map(str::to_string);
                extended_activity(engine, pid, s, t, &body);
                guard(c, s, |c, s| parse_msg(c, s, &body))
            }
            b'B' => {
                extended_activity(engine, pid, s, t, &body);
                guard(c, s, |c, s| bind_msg(c, s, &body))
            }
            b'D' => guard(c, s, |c, s| describe_msg(c, s, &body)),
            b'E' => {
                extended_activity(engine, pid, s, t, &body);
                guard(c, s, |c, s| execute_msg(c, s, &body))
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
                statement_done_activity(engine, pid, s);
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
                start_activity(engine, pid, s, "fastpath function call", None);
                let r = guard(c, s, |c, s| fastpath_msg(c, s, &body));
                send_notices(c, s);
                if let Err(e) = r {
                    c.error(&e);
                }
                statement_done_activity(engine, pid, s);
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
            // Any ERROR aborts the transaction (a protocol one too).
            s.on_error();
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

/// pg_stat_activity while extended-protocol messages run (pgstat_report_activity in
/// exec_parse_message / exec_bind_message / exec_execute_message): active, with the
/// statement's text, until the Sync's ReadyForQuery.
fn extended_activity(engine: &Engine, pid: i32, s: &Session, t: u8, body: &[u8]) {
    let mut b = Body::new(body);
    let sql = match t {
        b'P' => b.cstr().ok().and_then(|_| b.cstr().ok()).map(str::to_string),
        b'B' => b
            .cstr()
            .ok()
            .and_then(|_| b.cstr().ok())
            .and_then(|stmt| s.prepared.get(stmt).map(|p| p.sql.clone())),
        _ => b
            .cstr()
            .ok()
            .and_then(|portal| s.portals.get(portal).map(|p| p.stmt.sql.clone())),
    };
    start_activity(engine, pid, s, "active", sql);
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
    s.note_idle();
    c.at_ready = true;
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

/// Write a message (type `t`, the body `body` writes) to `b`.
fn put_msg<T>(b: &mut BytesMut, t: u8, body: impl FnOnce(&mut BytesMut) -> T) -> T {
    b.put_u8(t);
    let at = b.len();
    b.put_i32(0);
    let r = body(b);
    let len = (b.len() - at) as i32;
    b[at..at + 4].copy_from_slice(&len.to_be_bytes());
    r
}

fn put_row_description(b: &mut BytesMut, cols: &[OutCol], formats: &[i16]) {
    put_msg(b, b'T', |b| {
        b.put_i16(cols.len() as i16);
        for (i, col) in cols.iter().enumerate() {
            cstr(b, &col.name);
            b.put_u32(col.table);
            b.put_i16(col.attnum);
            b.put_u32(col.type_oid());
            b.put_i16(col.ty.typlen());
            b.put_i32(col.ty.catalog_typmod());
            b.put_i16(result_format(formats, i));
        }
    });
}

fn row_description(c: &mut Conn, cols: &[OutCol], formats: &[i16]) {
    put_row_description(&mut c.out, cols, formats);
}

fn result_format(formats: &[i16], i: usize) -> i16 {
    match formats.len() {
        0 => 0,
        1 => formats[0],
        _ => formats.get(i).copied().unwrap_or(0),
    }
}

/// A DataRow (printtup). A value its output function or the client's encoding can't take
/// fails the row: nothing of it is left in `b`.
fn put_data_row(b: &mut BytesMut, row: &[crate::types::Value], cols: &[OutCol], formats: &[i16], fmt: &FormatCtx) -> Result<()> {
    let start = b.len();
    let r = put_msg(b, b'D', |b| {
        b.put_i16(row.len() as i16);
        for (i, v) in row.iter().enumerate() {
            if v.is_null() {
                b.put_i32(-1);
                continue;
            }
            let text = crate::types::DataType::Text;
            let ty = cols.get(i).map_or(&text, |c| &c.ty);
            let at = b.len();
            b.put_i32(0);
            if result_format(formats, i) == 1 {
                crate::types::binary::encode(v, ty, fmt, b)?;
            } else {
                b.put_slice(&to_client(&format_value(v, ty, fmt))?);
            }
            let n = (b.len() - at - 4) as i32;
            b[at..at + 4].copy_from_slice(&n.to_be_bytes());
        }
        Ok(())
    });
    if r.is_err() {
        b.truncate(start);
    }
    r
}

/// The rows a statement collected (none, when they streamed: `WireSink`).
fn data_rows(c: &mut Conn, s: &Session, r: &QueryResult, formats: &[i16]) -> Result<()> {
    if r.rows.is_empty() {
        return Ok(());
    }
    let fmt = s.format_ctx();
    let cols = r.cols.as_deref().unwrap_or(&[]);
    for row in &r.rows {
        put_data_row(&mut c.out, row, cols, formats, &fmt)?;
        c.maybe_flush()?;
    }
    Ok(())
}

/// A statement's rows (and COPY TO's data) on their way to the client as the executor makes
/// them: into the buffer parked while the statement runs (`UNFLUSHED`), behind what the
/// connection had buffered and in order with the notices it raises, and to the socket each
/// time 8 kB have piled up (PostgreSQL's send buffer, PQ_SEND_BUFFER_SIZE). The client has
/// the rows as they're made and the rows before an error; memory stays bounded for any
/// result: a client that stops reading stops the statement, as a full socket does a backend.
struct WireSink {
    wr: std::rc::Rc<TcpStream>,
    /// The portal's result formats (an Execute's); empty: text, and a simple query's
    /// RowDescription goes out first.
    formats: Vec<i16>,
    describe: bool,
}

impl WireSink {
    fn put(&mut self, f: impl FnOnce(&mut BytesMut) -> Result<()>) -> Result<()> {
        UNFLUSHED.with(|u| {
            let mut b = u.borrow_mut();
            f(&mut b)?;
            if b.len() >= 8 << 10 {
                (&*self.wr)
                    .write_all(&b)
                    .map_err(|e| crate::session::sink::connection_lost(&e))?;
                b.clear();
            }
            Ok(())
        })
    }
}

impl crate::session::RowSink for WireSink {
    fn begin(&mut self, cols: &[OutCol]) -> Result<()> {
        if self.describe {
            self.put(|b| {
                put_row_description(b, cols, &[]);
                Ok(())
            })?;
        }
        Ok(())
    }

    fn row(&mut self, row: crate::exec::Row, cols: &[OutCol], fmt: &FormatCtx) -> Result<()> {
        let formats = std::mem::take(&mut self.formats);
        let r = self.put(|b| put_data_row(b, &row, cols, &formats, fmt));
        self.formats = formats;
        r
    }

    fn takes_copy(&self) -> bool {
        true
    }

    fn copy_begin(&mut self, binary: bool, ncols: usize) -> Result<()> {
        let f = if binary { 1 } else { 0 };
        self.put(|b| {
            put_msg(b, b'H', |b| {
                b.put_i8(f);
                b.put_i16(ncols as i16);
                for _ in 0..ncols {
                    b.put_i16(f as i16);
                }
            });
            Ok(())
        })
    }

    fn copy_data(&mut self, data: &[u8]) -> Result<()> {
        self.put(|b| {
            put_msg(b, b'd', |b| b.put_slice(data));
            Ok(())
        })
    }

    fn copy_end(&mut self) -> Result<()> {
        self.put(|b| {
            put_msg(b, b'c', |_| {});
            Ok(())
        })
    }
}

/// Run `f` (a statement) with its rows streaming to the client (`WireSink`).
fn streamed<T>(c: &mut Conn, s: &mut Session, formats: &[i16], describe: bool, f: impl FnOnce(&mut Session) -> T) -> T {
    s.row_sink = Some(Box::new(WireSink { wr: c.live.clone(), formats: formats.to_vec(), describe }));
    let r = c.unflushed(|| f(s));
    s.row_sink = None;
    r
}

fn command_complete(c: &mut Conn, tag: &str) {
    c.msg(b'C', |b| cstr(b, tag));
}

fn simple_query(c: &mut Conn, s: &mut Session, sql: &str) -> std::io::Result<()> {
    let start = std::time::Instant::now();
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
    // log_statement: the whole query string once (exec_simple_query).
    let logged = s.statement_logged(&stmts);
    if logged {
        let detail = s.execute_detail(&stmts);
        s.log_message(&format!("statement: {sql}"), detail.as_deref());
        send_notices(c, s);
    }
    s.set_batch_len(stmts.len());
    let sent = c.sent.len();
    let r = simple_batch(c, s, sql, &stmts);
    s.set_batch_len(0);
    // check_log_duration (not for a statement that failed).
    let failed = c.sent[sent.min(c.sent.len())..].iter().any(|e| e.severity != crate::error::Severity::Warning);
    if !failed && let Some((ms, with_stmt)) = s.duration_logged(start.elapsed(), logged) {
        if with_stmt {
            let detail = s.execute_detail(&stmts);
            s.log_message(&format!("duration: {ms} ms  statement: {sql}"), detail.as_deref());
        } else {
            s.log_message(&format!("duration: {ms} ms"), None);
        }
        send_notices(c, s);
    }
    r
}

fn simple_batch(
    c: &mut Conn,
    s: &mut Session,
    sql: &str,
    stmts: &[Arc<Prepared>],
) -> std::io::Result<()> {
    for (i, p) in stmts.iter().enumerate() {
        // (What came before, the scanner's warnings say, goes before what the statement
        // raises as it runs.)
        send_notices(c, s);
        let mut out = streamed(c, s, &[], true, |s| s.execute(p));
        // Like Postgres, an implicit transaction commits before the last command tag goes out,
        // so deferred constraint failures replace it.
        if i + 1 == stmts.len()
            && matches!(out, Ok(Outcome::Done(_)))
            && let Err(e) = s.end_batch()
        {
            // (The rows went out as the statement ran, before the commit failed: what
            // didn't stream goes now.)
            if let Ok(Outcome::Done(r)) = out
                && r.cols.is_some()
            {
                if s.sent.is_none() {
                    row_description(c, r.cols.as_deref().unwrap_or(&[]), &[]);
                }
                let _ = data_rows(c, s, &r, &[]);
            }
            out = Err(e);
        }
        send_notices(c, s);
        // (The rows a SELECT made before failing went out before the error.)
        let r = out.and_then(|o| outcome(c, s, o, &[]));
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
                && s.sent.is_none()
            {
                row_description(c, cols, &[]);
            }
            data_rows(c, s, &r, formats)?;
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
                // A cancel while the rows go out stops them, as the executor's
                // CHECK_FOR_INTERRUPTS would.
                if s.cancel.load(Ordering::Relaxed) {
                    s.on_error();
                    return Err(Error::new(
                        code::QUERY_CANCELED,
                        "canceling statement due to user request",
                    ));
                }
            }
            if let Some(e) = out.error {
                s.on_error();
                return Err(e);
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
            // The data is read as the insert takes it (CopyFrom reads lines as the client
            // sends them): a bad line fails the COPY while the client is still sending, and
            // only a batch of rows is held at a time.
            let mut read = || -> Result<Option<Vec<u8>>> {
                loop {
                    match c.read_message()? {
                        Some((b'd', body)) => return Ok(Some(body)),
                        Some((b'c', _)) => return Ok(None),
                        Some((b'f', body)) => {
                            let msg = Body::new(&body).cstr().unwrap_or("").to_string();
                            return Err(Error::new(
                                code::QUERY_CANCELED,
                                format!("COPY from stdin failed: {msg}"),
                            ));
                        }
                        Some((b'H' | b'S', _)) => {}
                        Some((t, _)) => {
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
            };
            let r = s.copy_in_stream(cin, &mut read);
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
    let n = b.i16()? as u16 as usize;
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
    let nf = b.i16()? as u16 as usize;
    let mut formats = Vec::with_capacity(nf);
    for _ in 0..nf {
        formats.push(b.i16()?);
    }
    let np = b.i16()? as u16 as usize;
    let mut params = Vec::with_capacity(np);
    for _ in 0..np {
        let len = b.i32()?;
        params.push(if len < 0 {
            None
        } else {
            Some(b.bytes(len as usize)?.to_vec())
        });
    }
    let nr = b.i16()? as u16 as usize;
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
                let oids = p.param_type_oids();
                o.put_i16(oids.len() as i16);
                for t in oids {
                    o.put_u32(t);
                }
            });
            match cols {
                Some(cols) => row_description(c, &cols, &[]),
                None => c.msg(b'n', |_| {}),
            }
        }
        b'P' => {
            let Some((cols, f)) = s.portal_cols(&name) else {
                return Err(Error::new(
                    code::INVALID_CURSOR_NAME,
                    format!("portal \"{name}\" does not exist"),
                ));
            };
            match cols {
                Some(cols) => row_description(c, &cols, &f),
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
    send_notices(c, s);
    // exec_execute_message's log_statement and duration lines.
    let start = std::time::Instant::now();
    let info = if crate::log::active() || s.state.settings.borrow().get_str("client_min_messages") == Some("log") {
        s.portal_log(&portal)
    } else {
        None
    };
    let logged = info.as_ref().is_some_and(|(_, _, p)| s.statement_logged(std::slice::from_ref(p)));
    if let Some((what, detail, _)) = info.as_ref().filter(|_| logged) {
        s.log_message(&format!("execute {what}: {sql}"), detail.as_deref());
        send_notices(c, s);
    }
    let r = streamed(c, s, &formats, false, |s| s.execute_portal(&portal, max_rows));
    if r.is_ok() {
        s.note_executed();
        if let Some((what, detail, _)) = &info
            && let Some((ms, with_stmt)) = s.duration_logged(start.elapsed(), logged)
        {
            if with_stmt {
                s.log_message(&format!("duration: {ms} ms  execute {what}: {sql}"), detail.as_deref());
            } else {
                s.log_message(&format!("duration: {ms} ms"), None);
            }
        }
    }
    send_notices(c, s);
    let (o, more) = r.map_err(|e| e.in_chars(&sql))?;
    match o {
        Outcome::Done(r) if more => {
            data_rows(c, s, &r, &formats)?;
            c.msg(b's', |_| {});
        }
        // Rows only: Execute doesn't send a RowDescription.
        Outcome::Done(r) => {
            data_rows(c, s, &r, &formats)?;
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
    let nf = b.i16()? as u16 as usize;
    let mut formats = Vec::with_capacity(nf);
    for _ in 0..nf {
        formats.push(b.i16()?);
    }
    let nargs = b.i16()? as u16;
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

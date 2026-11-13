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
/// PG_PROTOCOL_LATEST: 3.2, whose BackendKeyData carries a longer cancel key.
const PROTOCOL_LATEST: i32 = (3 << 16) | 2;
/// PQ_LARGE_MESSAGE_LIMIT (MaxAllocSize - 1) and PQ_SMALL_MESSAGE_LIMIT: the longest a
/// message may say it is, by its type.
const LARGE_MESSAGE: usize = 0x3fff_fffe;
const SMALL_MESSAGE: usize = 10000;
/// MAX_STARTUP_PACKET_LENGTH: the longest startup packet (after its length word).
const MAX_STARTUP_PACKET: usize = 10000;

/// SocketBackend: the frontend messages there are, and the longest each may be; None: not
/// one (the connection has lost track of where messages begin).
fn frontend_limit(t: u8) -> Option<usize> {
    match t {
        b'Q' | b'F' | b'B' | b'P' | b'd' => Some(LARGE_MESSAGE),
        b'X' | b'C' | b'D' | b'E' | b'H' | b'S' | b'c' | b'f' => Some(SMALL_MESSAGE),
        _ => None,
    }
}

/// CopyGetData: the messages COPY FROM STDIN reads, and their limits.
fn copy_in_limit(t: u8) -> Option<usize> {
    match t {
        b'd' => Some(LARGE_MESSAGE),
        b'c' | b'f' | b'H' | b'S' => Some(SMALL_MESSAGE),
        _ => None,
    }
}

/// Any message type, as large as a message may be (a walsender's standby messages).
fn any_limit(_: u8) -> Option<usize> {
    Some(LARGE_MESSAGE)
}

/// `read_message_into` found a message type its reader doesn't take (the type; its length
/// word unread).
#[derive(Debug)]
struct UnexpectedType(u8);

impl std::fmt::Display for UnexpectedType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unexpected message type {}", self.0)
    }
}

impl std::error::Error for UnexpectedType {}

/// pq_getmessage's COMMERROR: a length word out of range, or the message cut short. Nothing
/// goes to the client; the connection ends.
fn invalid_length() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid message length")
}

/// The FATAL that ends a connection whose messages can't be told apart any more (an error
/// while a message was half read: PostgresMain's error recovery).
fn sync_lost() -> Error {
    Error::new(code::PROTOCOL_VIOLATION, "terminating connection because protocol synchronization was lost").fatal()
}
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
        // (log_connections' setup_durations: "fork" is this thread's start, the backend's.)
        let accepted = std::time::Instant::now();
        let engine = engine.clone();
        let opts = opts.clone();
        std::thread::Builder::new()
            .name("roll-conn".into())
            .stack_size(THREAD_STACK)
            .spawn(move || {
                let peer = stream.peer_addr().ok();
                if let Err(e) = handle(engine, stream, peer, &opts, accepted) {
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
    // Noted before this returns: a session opened right after (embedded, not through the
    // listener) reads `port` and `listen_addresses` from it, and the accept thread may not
    // have started yet (`SHOW port` read 5432 now and then under load).
    engine.note_listen_addr(local);
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

/// How long a connection's thread polls its socket for the client's next message before it
/// sleeps in the read (`set_wait_spin`; zero: it sleeps at once).
static WAIT_SPIN_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Connection threads at work (not waiting for their clients) and threads polling their
/// sockets, counted while a spin is set: a thread polls only while both together fit in a
/// quarter of the cores the process may run on (on 4 cores: only while no other connection
/// works or polls), so polling never takes the core a woken thread needs (it counts as
/// working only once it runs). On 4 cores, two pollers at most cost 32 clients 15% of their
/// throughput; working and polling threads up to all 4 cores cost 8 clients 8%, up to 2
/// cost 8 clients' range reads 9%.
/// (On a cache line of their own: every connection writes them, and the flags and settings
/// next to them in memory are read by every statement.)
#[repr(align(128))]
struct Counts {
    working: std::sync::atomic::AtomicUsize,
    spinning: std::sync::atomic::AtomicUsize,
}
static COUNTS: Counts = Counts { working: std::sync::atomic::AtomicUsize::new(0), spinning: std::sync::atomic::AtomicUsize::new(0) };
static WORKING: &std::sync::atomic::AtomicUsize = &COUNTS.working;
static SPINNING: &std::sync::atomic::AtomicUsize = &COUNTS.spinning;
static QUARTER_CORES: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

/// Poll for a client's next message up to this long before sleeping, while cores are free
/// (performance mode's): a reply that comes back within it skips the sleep and the wake-up.
/// Set before serving.
pub fn set_wait_spin(d: std::time::Duration) {
    WAIT_SPIN_NS.store(d.as_nanos().min(u64::MAX as u128) as u64, Ordering::Relaxed);
}

/// A connection thread out of `WORKING` while it waits for its client.
struct Waiting(bool);

impl Waiting {
    fn new(counted: bool) -> Waiting {
        if counted {
            WORKING.fetch_sub(1, Ordering::Relaxed);
        }
        Waiting(counted)
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        if self.0 {
            WORKING.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The client's next bytes if they come within `set_wait_spin`'s time while cores are free
/// (None: they didn't, or no spin is set, or no core is free).
fn spin_read(s: &TcpStream, buf: &mut [u8]) -> Option<std::io::Result<usize>> {
    use std::os::fd::AsRawFd;
    let ns = WAIT_SPIN_NS.load(Ordering::Relaxed);
    if ns == 0 || buf.is_empty() {
        return None;
    }
    let quarter = *QUARTER_CORES.get_or_init(|| std::thread::available_parallelism().map_or(1, |n| (n.get() / 4).max(1)));
    let fits = || WORKING.load(Ordering::Relaxed) + SPINNING.load(Ordering::Relaxed) <= quarter;
    SPINNING.fetch_add(1, Ordering::Relaxed);
    let start = std::time::Instant::now();
    let r = loop {
        if !fits() {
            break None;
        }
        // SAFETY: recv writes at most buf.len() bytes into buf, a live exclusive slice.
        let n = unsafe { libc::recv(s.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), libc::MSG_DONTWAIT) };
        if n >= 0 {
            break Some(Ok(n as usize));
        }
        let e = std::io::Error::last_os_error();
        match e.kind() {
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted => {}
            _ => break Some(Err(e)),
        }
        if start.elapsed().as_nanos() as u64 >= ns {
            break None;
        }
        std::hint::spin_loop();
    };
    SPINNING.fetch_sub(1, Ordering::Relaxed);
    r
}

/// A connection's reading side: a read that waits past `IDLE_TRIM` runs the idle hook (once
/// until the client sends something again) and goes on waiting; one waiting past `deadline`
/// (an idle timeout) fails with `IdleTimeout`.
struct IdleRead {
    s: Arc<TcpStream>,
    armed: bool,
    deadline: Option<std::time::Instant>,
    /// Counted in `WORKING` (a spin was set when the connection came).
    counted: bool,
    /// Poll before the next wait: the client's last message came within the spin's time. A
    /// client slower than that (8 clients' range reads, each checking 100 rows) would only
    /// have its server threads poll for nothing: 7% fewer reads, 11% more CPU per read.
    spin: bool,
}

impl Drop for IdleRead {
    fn drop(&mut self) {
        if self.counted {
            WORKING.fetch_sub(1, Ordering::Relaxed);
        }
    }
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
    fn new(s: Arc<TcpStream>) -> std::io::Result<IdleRead> {
        if IDLE_HOOK.get().is_some() {
            s.set_read_timeout(Some(IDLE_TRIM))?;
        }
        let counted = WAIT_SPIN_NS.load(Ordering::Relaxed) != 0;
        if counted {
            WORKING.fetch_add(1, Ordering::Relaxed);
        }
        Ok(IdleRead { s, armed: true, deadline: None, counted, spin: counted })
    }

    /// Wake for the pending idle trim or a session deadline; otherwise block for data.
    fn wait_for(&self) -> Option<std::time::Duration> {
        let hook = IDLE_HOOK.get().filter(|_| self.armed).map(|_| IDLE_TRIM);
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
        let _waiting = Waiting::new(self.counted);
        if self.deadline.is_some() {
            self.s.set_read_timeout(self.wait_for())?;
        }
        // (How long the client takes, for whether to poll next time.)
        let began = self.counted.then(std::time::Instant::now);
        if let Some(r) = self.spin.then(|| spin_read(&self.s, buf)).flatten() {
            let n = r?;
            if !std::mem::replace(&mut self.armed, true) {
                self.s.set_read_timeout(self.wait_for())?;
            }
            return Ok(n);
        }
        loop {
            match (&*self.s).read(buf) {
                Ok(n) => {
                    if let Some(b) = began {
                        self.spin = b.elapsed().as_nanos() as u64 <= WAIT_SPIN_NS.load(Ordering::Relaxed);
                    }
                    if !std::mem::replace(&mut self.armed, true) {
                        self.s.set_read_timeout(self.wait_for())?;
                    }
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
                    self.s.set_read_timeout(self.wait_for())?;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

// ── framing ──

struct Conn {
    rd: BufReader<IdleRead>,
    /// The connection's one socket, which the reader, notices, notifications and the
    /// backend table share (PostgreSQL's backend has one descriptor per connection).
    wr: Arc<TcpStream>,
    out: BytesMut,
    /// Where the message being written starts (to drop it if a panic cuts it short).
    partial: Option<usize>,
    /// A FATAL error went out: the session ends.
    fatal: bool,
    /// pg_terminate_backend was called on this session.
    terminated: Option<Arc<AtomicBool>>,
    /// The backend's count of messages read (`Engine::read_counter`).
    reads: Option<Arc<std::sync::atomic::AtomicU64>>,
    /// ReadyForQuery went out and no message came since: the session is idle, and its idle
    /// timeouts run (only then: not between the messages of an extended query).
    at_ready: bool,
    /// Errors and warnings sent, for the server log (kept only while it's written).
    sent: Vec<Error>,
    /// What the server log reads of the connection, and when it started (MyStartTime).
    log: crate::log::Conn,
    started: i64,
    /// The backend's pid and the server's log format once it is registered: the errors it
    /// sends before its session exists are logged with them (`Drop`).
    pre_session_log: Option<(i32, crate::log::Format)>,
    /// A walsender connection (`replication=...` in the startup packet).
    repl: Option<crate::repl::walsender::Mode>,
    /// The snapshot its last CREATE_REPLICATION_SLOT exported.
    exported: Option<crate::repl::walsender::Exported>,
    /// ErrorResponses sent so far.
    errors: u64,
    /// A message was cut off where an error stopped reading it (COPY FROM STDIN's
    /// unexpected message): once the error is out, the connection ends (`sync_lost`).
    sync_lost: bool,
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
        self.read_message(any_limit)?.map(Some).ok_or_else(eof)
    }
}

impl Conn {
    fn read_i32(&mut self) -> std::io::Result<i32> {
        let mut b = [0u8; 4];
        self.rd.read_exact(&mut b)?;
        Ok(i32::from_be_bytes(b))
    }

    /// A message of a type `limit` takes: (type, body).
    fn read_message(&mut self, limit: impl Fn(u8) -> Option<usize>) -> std::io::Result<Option<(u8, Vec<u8>)>> {
        let mut body = Vec::new();
        Ok(self.read_message_into(None, &mut body, limit)?.map(|t| (t, body)))
    }

    /// `read_message`, its body read into `body` (the message loop's, its allocation reused
    /// from message to message), its first byte awaited no later than `deadline` (an idle
    /// timeout: the read fails with `IdleTimeout`). The message's type. A type `limit`
    /// doesn't take fails with `UnexpectedType` before its length is read; a length past
    /// the type's limit, or a message cut short, with `invalid_length`.
    fn read_message_into(
        &mut self,
        deadline: Option<std::time::Instant>,
        body: &mut Vec<u8>,
        limit: impl Fn(u8) -> Option<usize>,
    ) -> std::io::Result<Option<u8>> {
        let mut t = [0u8; 1];
        self.rd.get_mut().deadline = deadline;
        let first = self.rd.read(&mut t);
        if deadline.is_some() {
            let r = self.rd.get_mut();
            r.deadline = None;
            r.s.set_read_timeout(r.wait_for())?;
        }
        if first? == 0 {
            return Ok(None);
        }
        let Some(max) = limit(t[0]) else {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, UnexpectedType(t[0])));
        };
        let len = self.read_i32().map_err(|_| invalid_length())? as isize;
        if len < 4 || len as usize > max {
            return Err(invalid_length());
        }
        body.clear();
        body.resize(len as usize - 4, 0);
        self.rd.read_exact(body).map_err(|_| invalid_length())?;
        Ok(Some(t[0]))
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
            (&*self.wr).write_all(&self.out)?;
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
        self.errors += 1;
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
        let mut sent = LIVE_SENT.with(|l| std::mem::take(&mut *l.borrow_mut()));
        sent.append(&mut self.sent);
        if let Some(s) = s {
            sent.append(&mut s.state.log_only.borrow_mut());
            // The temporary files the statement removed past log_temp_files
            // (ReportTemporaryFileUsage's LOG, with the statement).
            for (path, size) in s.state.temp.take_logged() {
                let mut e = Error::new("00000", format!("temporary file: path \"{path}\", size {size}"));
                e.severity = crate::error::Severity::Log;
                sent.push(e);
            }
        }
        for e in sent {
            match s {
                Some(s) => s.log_error(&e, statement),
                None => {
                    let f = crate::log::Format::postmaster();
                    let who = crate::log::Who::backend(pid, self.started, &self.log);
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
    /// Warnings sent as a statement ran (the live notice sink), for the server log.
    static LIVE_SENT: std::cell::RefCell<Vec<Error>> = const { std::cell::RefCell::new(Vec::new()) };
    /// The session's client_encoding and its database's encoding (sessions run on threads
    /// of their own): text goes out in the first and query text comes in in it.
    static CLIENT_ENC: std::cell::Cell<(i32, i32)> =
        const { std::cell::Cell::new((crate::types::mbconv::UTF8, crate::types::mbconv::UTF8)) };
}

/// Take up the session's client_encoding for the messages that follow.
fn sync_client_encoding(s: &Session) {
    let e = s.state.settings.borrow().encodings();
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

/// An immediate shutdown (SIGQUIT): quickdie's WARNING_CLIENT_ONLY to every client, whose
/// connection then closes (PostgreSQL's backends exit right after it).
pub fn quit_clients(engine: &Engine) {
    let e = Error {
        severity: crate::error::Severity::Warning,
        ..Error::new(code::ADMIN_SHUTDOWN, "terminating connection due to immediate shutdown command")
    };
    let mut b = BytesMut::new();
    b.put_u8(b'N');
    b.put_i32(0);
    error_fields(&mut b, &e, e.severity.as_str());
    let len = (b.len() - 1) as i32;
    b[1..5].copy_from_slice(&len.to_be_bytes());
    engine.quit_client_backends(&b);
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

    /// pq_getmsgend: nothing may be left of the message.
    fn end(&self) -> Result<()> {
        if self.pos != self.b.len() {
            return Err(Body::bad());
        }
        Ok(())
    }
}

// ── connection ──

/// Deregisters the backend when the connection ends, however it ends.
struct BackendGuard<'a> {
    engine: &'a Engine,
    pid: i32,
}

/// A backend that ends before its session exists (authentication failed, no such role or
/// database, a bad startup option) logs the FATAL it sent, as PostgreSQL's does.
impl Drop for Conn {
    fn drop(&mut self) {
        if let Some((pid, f)) = &self.pre_session_log {
            for e in std::mem::take(&mut self.sent) {
                let who = crate::log::Who::backend(*pid, self.started, &self.log);
                crate::log::write(f, &who, &crate::log::Message::error(&e, None));
            }
        }
    }
}

impl Conn {
    /// A LOG line of the backend before its session exists, under the server's settings.
    fn log_pre_session(&self, text: &str) {
        if let Some((pid, f)) = &self.pre_session_log {
            let who = crate::log::Who::backend(*pid, self.started, &self.log);
            crate::log::write(f, &who, &crate::log::Message::log(text));
        }
    }
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
    accepted: std::time::Instant,
) -> std::io::Result<()> {
    let forked = std::time::Instant::now();
    crate::stack::set_limit(THREAD_STACK - (2 << 20));
    // (The connection's thread keeps its own copies of what every session reads.)
    crate::published::keep_on_this_thread();
    stream.set_nodelay(true)?;
    let stream = Arc::new(stream);
    let mut c = Conn {
        rd: BufReader::with_capacity(16 << 10, IdleRead::new(stream.clone())?),
        wr: stream.clone(),
        out: BytesMut::with_capacity(8 << 10),
        partial: None,
        fatal: false,
        terminated: None,
        reads: None,
        at_ready: false,
        sent: Vec::new(),
        repl: None,
        exported: None,
        errors: 0,
        sync_lost: false,
        log: crate::log::Conn::new(peer, stream.local_addr().ok(), false),
        started: crate::types::datetime::Clock::system().now,
        pre_session_log: None,
    };
    // Notices a statement raises before it waits for a lock go out then, as Postgres
    // flushes each notice: after what the connection had buffered (`Conn::unflushed`).
    let live = stream.clone();
    crate::exec::set_notice_sink(Some(Box::new(move |ns: &[Error]| {
        let mut b = UNFLUSHED.with(|u| std::mem::take(&mut *u.borrow_mut()));
        if crate::log::active() {
            LIVE_SENT.with(|l| {
                l.borrow_mut().extend(ns.iter().filter(|e| e.severity == crate::error::Severity::Warning).cloned())
            });
        }
        for e in ns {
            b.put_u8(b'N');
            let at = b.len();
            b.put_i32(0);
            error_fields(&mut b, e, e.severity.as_str());
            let len = (b.len() - at) as i32;
            b[at..at + 4].copy_from_slice(&len.to_be_bytes());
        }
        let _ = (&*live).write_all(&b);
    })));
    struct NoSink;
    impl Drop for NoSink {
        fn drop(&mut self) {
            crate::exec::set_notice_sink(None);
        }
    }
    let _no_sink = NoSink;

    // Startup: SSL / GSS negotiation and cancel requests come first (ProcessStartupPacket).
    let (proto, params) = match startup_packet(&mut c, &engine)? {
        Some(p) => p,
        None => return c.flush(),
    };

    // log_connections' receipt (the files' and the command line's value: the session's
    // settings aren't there yet).
    let lc = engine
        .command_line_value("log_connections")
        .map(str::to_string)
        .or_else(|| engine.server_conf().loaded().values.get("log_connections").map(|v| v.value.clone()))
        .unwrap_or_default();
    let lc = crate::log::ConnAspects::of(&lc);
    // The engine's connections to itself through libpq's default socket stand for Unix-socket
    // ones: no client or server address ("[local]"), pg_hba.conf's `local` lines.
    let unix = peer.is_some_and(crate::hba::take_local);
    let peer = peer.filter(|_| !unix);
    if unix {
        c.log = crate::log::Conn::new(None, None, true);
    }
    // (A 3.2 client's cancel key is MAX_CANCEL_KEY_LENGTH bytes long, an older one's 4.)
    let key_len = if proto >= PROTOCOL_LATEST { 32 } else { 4 };
    let (pid, key, terminated) = engine.register_backend(Some(stream.clone()), peer, key_len);
    let _guard = BackendGuard {
        engine: &engine,
        pid,
    };
    c.terminated = Some(terminated.clone());
    c.reads = engine.read_counter(pid);
    if crate::log::active() {
        let files = engine.server_conf().loaded();
        c.pre_session_log = Some((pid, crate::log::Format::from_files(Some(&files))));
    }
    if lc.receipt {
        let host = match peer {
            Some(p) => format!("{} port={}", p.ip().to_canonical(), p.port()),
            None => "[local]".into(),
        };
        c.log_pre_session(&format!("connection received: host={host}"));
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

    let physical = c.repl == Some(crate::repl::walsender::Mode::Physical);
    // (A physical walsender has no database: %d "[unknown]", "database=" at its end.)
    let log_database = if physical { String::new() } else { database.clone() };
    c.log.walsender = c.repl.is_some();
    c.log.startup = Some(crate::log::Startup { user: user.clone(), database: log_database.clone(), ps: "", vxid: String::new() });
    // (ProcessStartupPacket's canAcceptConnections: a smart or fast shutdown is under way.)
    if engine.refusing_connections() {
        c.error(&Error::new(code::CANNOT_CONNECT_NOW, "the database system is shutting down").fatal());
        return c.flush();
    }
    // InitProcess: a walsender's slot comes from the walsenders' list.
    let kind = if c.repl.is_some() { crate::txn::procs::ProcKind::WalSender } else { crate::txn::procs::ProcKind::Backend };
    let proc = engine.assign_proc(pid, kind);
    if let Some(s) = c.log.startup.as_mut() {
        s.ps = "authentication";
        s.vxid = proc.current().to_string();
    }
    let auth_start = std::time::Instant::now();
    let (method, hba_line, hba_raw) = match hba_method(&engine, peer, unix, &user, &database, physical, opts.auth) {
        Ok(m) => m,
        Err(e) => {
            c.error(&e);
            return c.flush();
        }
    };
    let hba_file = || {
        engine.server_conf().auth_files().hba_file.as_ref().map_or_else(
            || crate::session::settings::Settings::new().get_raw("hba_file").unwrap_or_default(),
            |p| p.to_string_lossy().into_owned(),
        )
    };
    let mut why = None;
    if let Err(e) = authenticate(&mut c, &engine, &user, &database, method, &mut why) {
        auth_delay(&engine);
        c.error(&e);
        // auth_failed's errdetail_log: why, and the rule the connection matched.
        if e.code == code::INVALID_PASSWORD
            && let Some(logged) = c.sent.last_mut()
        {
            let matched = format!("Connection matched file \"{}\" line {hba_line}: \"{hba_raw}\"", hba_file());
            logged.detail = Some(match why {
                Some(w) => format!("{w}\n{matched}"),
                None => matched,
            });
        }
        return c.flush();
    }
    let auth_time = auth_start.elapsed();
    // (ClientAuthentication's AuthenticationOk: what fails after it, a missing database or
    // role, a bad setting, follows it.)
    c.auth(0, &[]);
    let app = get("application_name").unwrap_or("").to_string();
    if lc.authentication {
        // (set_authn_id names the identity a password proved; trust proves none.)
        let who = if method == AuthMethod::Trust { "user" } else { "identity" };
        let file = hba_file();
        c.log_pre_session(&format!(
            "connection authenticated: {who}=\"{user}\" method={} ({file}:{hba_line})",
            method.name()
        ));
    }
    if lc.authorization {
        // PerformAuthentication's line: the startup packet's application_name, if it had one
        // (pg_clean_ascii'd), before any setting or role check applies.
        let mut line = if c.repl.is_some() {
            format!("replication connection authorized: user={user}")
        } else {
            format!("connection authorized: user={user} database={database}")
        };
        if let Some(a) = get("application_name") {
            line.push_str(" application_name=");
            for b in a.bytes() {
                match b {
                    32..=126 => line.push(b as char),
                    _ => line.push_str(&format!("\\x{b:02x}")),
                }
            }
        }
        c.log_pre_session(&line);
    }
    if let Some(s) = c.log.startup.as_mut() {
        s.ps = "startup";
    }
    if let Err(e) = check_role_login(&engine, &user, &database) {
        c.error(&e);
        return c.flush();
    }
    let mut s = match engine.connect(pid, &database, &user, &app) {
        Ok(s) => s,
        Err(e) => {
            c.error(&e);
            return c.flush();
        }
    };
    s.reads_clear_cancel = true;
    s.state.settings.borrow_mut().tcp = !unix;
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
    let mut switch_error = None;
    crate::session::settings::take_name_notices();
    for (k, v) in &params {
        if k == "options" && switch_error.is_none() {
            let (set, err) = parse_options(v);
            settings.extend(set);
            switch_error = err;
        }
    }
    // (A switch's error comes once the switches before it are set, before the parameters.)
    let switches = settings.len();
    for (k, v) in &params {
        match k.as_str() {
            "user" | "database" | "replication" | "options" => {}
            k if k.starts_with("_pq_.") => {}
            k => settings.push((k.to_string(), v.clone())),
        }
    }
    for (i, (k, v)) in settings.into_iter().enumerate() {
        if i == switches
            && let Some(e) = switch_error.take()
        {
            c.error(&e.fatal());
            return c.flush();
        }
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
    if let Some(e) = switch_error {
        c.error(&e.fatal());
        return c.flush();
    }
    s.state.peer.set(peer);
    s.state.local_addr.set(c.wr.local_addr().ok().filter(|_| !unix));
    *s.state.log_conn.borrow_mut() = c.log.clone();
    s.preload_libraries();
    s.take_changed_settings();
    // (process_startup_options' NOTICEs: an application_name truncated.)
    let startup_notices = crate::session::settings::take_name_notices();
    // Notifications for an idle session go straight to the client.
    let wr = c.wr.clone();
    s.attach_notifications(Arc::new(move |notes: &[crate::notify::Notification]| {
        let mut b = BytesMut::new();
        for n in notes {
            notification(&mut b, n);
        }
        (&*wr).write_all(&b)
    }));

    for e in &startup_notices {
        c.notice(e);
    }
    {
        let mut st = s.state.settings.borrow_mut();
        for name in crate::session::settings::REPORTED {
            if st.exists(name) {
                let v = st.show_value(name);
                c.msg(b'S', |b| {
                    cstr(b, name);
                    cstr(b, &v);
                });
                st.reported.insert(name.to_string(), v);
            }
        }
    }
    c.msg(b'K', |b| {
        b.put_i32(pid);
        b.put_slice(key.bytes());
    });
    // Login event triggers, before the session is ready.
    if let Err(e) = s.run_login_triggers() {
        s.notices.push(e);
    }
    // log_connections' setup_durations (the session's value: the startup packet's options
    // count): PostgresMain's line at the first ReadyForQuery. The engine has no fork:
    // "fork" is from the accept to the connection's thread running, "authentication"
    // pg_hba.conf's choice and the password exchange, "total" from the accept to here.
    if crate::log::active()
        && crate::log::ConnAspects::of(&s.state.settings.borrow().get_raw("log_connections").unwrap_or_default())
            .setup_durations
    {
        let ms = |d: std::time::Duration| d.as_micros() as f64 / 1000.0;
        let line = format!(
            "connection ready: setup total={:.3} ms, fork={:.3} ms, authentication={:.3} ms",
            ms(accepted.elapsed()),
            ms(forked - accepted),
            ms(auth_time)
        );
        s.set_ps_idle();
        s.log_message(&line, None);
    }
    send_notices(&mut c, &mut s);
    ready_for_query(&mut c, &s)?;

    let started = std::time::Instant::now();
    let r = message_loop(&mut c, &engine, &mut s, pid, &terminated);
    // (An idle session's FATAL is logged too, before its disconnection line.)
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
    c.log_sent(Some(&s), pid, None);
    if s.state.settings.borrow().bool("log_disconnections") {
        // (The backend has left the proc array: no virtual transaction id.)
        s.log_exiting.set(true);
        let t = started.elapsed();
        let (h, m, sec, ms) = (t.as_secs() / 3600, t.as_secs() / 60 % 60, t.as_secs() % 60, t.subsec_millis());
        let host = match peer {
            Some(p) => format!("{} port={}", p.ip().to_canonical(), p.port()),
            None => "[local]".into(),
        };
        let line = format!(
            "disconnection: session time: {h}:{m:02}:{sec:02}.{ms:03} user={user} database={log_database} host={host}"
        );
        s.log_message(&line, None);
    }
    // Nothing more goes out: the client sees the end (FIN) right after the last message, as
    // when a backend exits, not after the session's teardown below (rollback, locks, the
    // backend's entry). In that window a client that had read a FATAL could send its next
    // query, still unread when the socket closed: a reset ("error communicating with the
    // server") instead of the end of the connection.
    let _ = c.wr.shutdown(std::net::Shutdown::Write);
    r
}

/// ProcessStartupPacket: SSLRequest and GSSENCRequest are refused ('N': the engine has
/// neither), a CancelRequest is passed on, and the startup packet's protocol version and
/// parameters come back. None: the connection ends (an error, if any, buffered).
#[allow(clippy::type_complexity)]
fn startup_packet(c: &mut Conn, engine: &Engine) -> std::io::Result<Option<(i32, Vec<(String, String)>)>> {
    let (mut ssl_done, mut gss_done) = (false, false);
    loop {
        // (A length word out of range, or a packet cut short: COMMERROR, nothing sent.)
        let Ok(len) = c.read_i32() else { return Ok(None) };
        let len = len as i64 - 4;
        if len < 4 || len > MAX_STARTUP_PACKET as i64 {
            return Ok(None);
        }
        let mut buf = vec![0u8; len as usize];
        if c.rd.read_exact(&mut buf).is_err() {
            return Ok(None);
        }
        let code = i32::from_be_bytes(buf[..4].try_into().unwrap());
        let refused = match code {
            CANCEL_REQUEST => {
                // ProcessCancelRequestPacket: the pid, then a key of 1 to 256 bytes.
                if buf.len() > 8 && buf.len() - 8 <= 256 {
                    let pid = i32::from_be_bytes(buf[4..8].try_into().unwrap());
                    engine.cancel_request(pid, &buf[8..]);
                }
                return Ok(None);
            }
            SSL_REQUEST if !ssl_done => {
                ssl_done = true;
                "received unencrypted data after SSL request"
            }
            GSSENC_REQUEST if !gss_done => {
                gss_done = true;
                "received unencrypted data after GSSAPI encryption request"
            }
            _ => "",
        };
        if !refused.is_empty() {
            (&*c.wr).write_all(b"N")?;
            // (What came with the request, before its answer, could have been injected.)
            if !c.rd.buffer().is_empty() {
                c.error(
                    &Error::new(code::PROTOCOL_VIOLATION, refused)
                        .detail("This could be either a client-software bug or evidence of an attempted man-in-the-middle attack.")
                        .fatal(),
                );
                return Ok(None);
            }
            continue;
        }
        let (major, minor) = (code as u32 >> 16, code as u32 & 0xffff);
        if major != 3 {
            let m = format!("unsupported frontend protocol {major}.{minor}: server supports 3.0 to 3.2");
            if major < 3 {
                // (FrontendProtocol is the client's: the error in the old protocol's form,
                // its text alone.)
                let mut b = Vec::with_capacity(m.len() + 12);
                b.extend_from_slice(b"EFATAL:  ");
                b.extend_from_slice(m.as_bytes());
                b.extend_from_slice(b"\n\0");
                (&*c.wr).write_all(&b)?;
            } else {
                c.error(&Error::new(code::FEATURE_NOT_SUPPORTED, m).fatal());
            }
            return Ok(None);
        }
        // Name / value pairs up to an empty name, which must be the packet's last byte (the
        // packet read as if a zero byte followed it).
        let at = |i: usize| -> &[u8] {
            let rest = &buf[i.min(buf.len())..];
            &rest[..rest.iter().position(|&b| b == 0).unwrap_or(rest.len())]
        };
        let mut params = Vec::new();
        let mut unknown = Vec::new();
        let mut offset = 4;
        while offset < buf.len() {
            let name = at(offset);
            if name.is_empty() {
                break;
            }
            let valoffset = offset + name.len() + 1;
            if valoffset >= buf.len() {
                break;
            }
            let value = at(valoffset);
            let name = String::from_utf8_lossy(name).into_owned();
            if name.starts_with("_pq_.") {
                unknown.push(name.clone());
            }
            params.push((name, String::from_utf8_lossy(value).into_owned()));
            offset = valoffset + value.len() + 1;
        }
        if offset != buf.len() - 1 {
            c.error(
                &Error::new(code::PROTOCOL_VIOLATION, "invalid startup packet layout: expected terminator as last byte")
                    .fatal(),
            );
            return Ok(None);
        }
        // A newer 3.x minor or _pq_ options: the newest minor spoken, and the options not
        // known (none are).
        if minor > 2 || !unknown.is_empty() {
            c.msg(b'v', |o| {
                o.put_i32(code.min(PROTOCOL_LATEST));
                o.put_i32(unknown.len() as i32);
                for u in &unknown {
                    cstr(o, u);
                }
            });
        }
        return Ok(Some((code, params)));
    }
}

/// process_postgres_switches of the startup `options` parameter (split as pg_split_opts
/// does: blanks between words, a backslash escaping the next character): the settings its
/// switches make, in order (`-c name=value`, `--name=value`, `-S` work_mem, `-B`
/// shared_buffers, ...), and the error that stops it, if any (the settings before it are
/// made first).
fn parse_options(s: &str) -> (Vec<(String, String)>, Option<Error>) {
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
    let syntax = |m: String| Error::new(code::SYNTAX_ERROR, m).routine("process_postgres_switches");
    let invalid = |arg: &str| {
        syntax(format!("invalid command-line argument for server process: {arg}"))
            .hint("Try \"postgres --help\" for more information.")
    };
    let mut out: Vec<(String, String)> = Vec::new();
    let set = |out: &mut Vec<(String, String)>, k: &str, v: &str| out.push((k.to_string(), v.to_string()));
    // (getopt's "B:bC:c:D:d:EeFf:h:ijk:lN:nOPp:r:S:sTt:v:W:-:"; non-options are permuted to
    // the end, where any one is an error.)
    let mut extra: Option<String> = None;
    let mut i = 0;
    while i < words.len() {
        let w = words[i].clone();
        i += 1;
        if w == "--" {
            break;
        }
        if !w.starts_with('-') || w == "-" {
            extra.get_or_insert(w);
            continue;
        }
        let flags: Vec<char> = w[1..].chars().collect();
        let mut j = 0;
        while j < flags.len() {
            let f = flags[j];
            j += 1;
            let takes = "BCcDdfhkNprStvW-".contains(f);
            if !takes && !"bEeFijlnOPsT".contains(f) {
                return (out, Some(invalid(&w)));
            }
            let arg = if takes {
                let rest: String = flags[j..].iter().collect();
                j = flags.len();
                if !rest.is_empty() {
                    rest
                } else if i < words.len() {
                    i += 1;
                    words[i - 1].clone()
                } else {
                    return (out, Some(invalid(&w)));
                }
            } else {
                String::new()
            };
            match f {
                'B' => set(&mut out, "shared_buffers", &arg),
                '-' | 'c' => {
                    if f == '-'
                        && (["check", "boot", "describe-config", "single"].contains(&arg.as_str()) || arg.starts_with("forkchild"))
                    {
                        return (out, Some(syntax(format!("--{arg} must be first argument"))));
                    }
                    match arg.split_once('=') {
                        Some((k, v)) => set(&mut out, &k.replace('-', "_"), v),
                        None if f == '-' => return (out, Some(syntax(format!("--{arg} requires a value")))),
                        None => return (out, Some(syntax(format!("-c {arg} requires a value")))),
                    }
                }
                'd' => {
                    let n: i64 = {
                        let t = arg.trim_start();
                        let d: String = t.chars().take_while(|c| c.is_ascii_digit() || *c == '-').collect();
                        d.parse().unwrap_or(0)
                    };
                    if n > 0 {
                        set(&mut out, "log_min_messages", &format!("debug{n}"));
                    } else {
                        set(&mut out, "log_min_messages", "notice");
                    }
                    for (lvl, k, v) in [(2, "log_statement", "all"), (3, "debug_print_parse", "true"), (4, "debug_print_plan", "true"), (5, "debug_print_rewritten", "true")] {
                        if n >= lvl {
                            set(&mut out, k, v);
                        }
                    }
                }
                'e' => set(&mut out, "datestyle", "euro"),
                'F' => set(&mut out, "fsync", "false"),
                'f' => {
                    let k = match arg.chars().next() {
                        Some('s') => "enable_seqscan",
                        Some('i') => "enable_indexscan",
                        Some('o') => "enable_indexonlyscan",
                        Some('b') => "enable_bitmapscan",
                        Some('t') => "enable_tidscan",
                        Some('n') => "enable_nestloop",
                        Some('m') => "enable_mergejoin",
                        Some('h') => "enable_hashjoin",
                        _ => return (out, Some(invalid(&w))),
                    };
                    set(&mut out, k, "false");
                }
                'h' => set(&mut out, "listen_addresses", &arg),
                'i' => set(&mut out, "listen_addresses", "*"),
                'k' => set(&mut out, "unix_socket_directories", &arg),
                'l' => set(&mut out, "ssl", "true"),
                'N' => set(&mut out, "max_connections", &arg),
                'O' => set(&mut out, "allow_system_table_mods", "true"),
                'P' => set(&mut out, "ignore_system_indexes", "true"),
                'p' => set(&mut out, "port", &arg),
                'S' => set(&mut out, "work_mem", &arg),
                's' => set(&mut out, "log_statement_stats", "true"),
                't' => {
                    let k = match (arg.chars().next(), arg.chars().nth(1)) {
                        (Some('p'), Some('a')) => "log_parser_stats",
                        (Some('p'), Some('l')) => "log_planner_stats",
                        (Some('e'), _) => "log_executor_stats",
                        _ => return (out, Some(invalid(&w))),
                    };
                    set(&mut out, k, "true");
                }
                'W' => set(&mut out, "post_auth_delay", &arg),
                // b, C, D, E, j, n, r, T, v: nothing from a client.
                _ => {}
            }
        }
    }
    if let Some(w) = extra.or_else(|| words.get(i).cloned()) {
        return (out, Some(invalid(&w)));
    }
    (out, None)
}

/// check_hba: the method pg_hba.conf's first matching rule names, with the rule's line
/// number and text (without rules, the server's `--auth` method for everyone, line 1 as
/// pg_hba_file_rules shows it); `reject` and no matching rule end the connection. `local`:
/// a connection standing for a Unix-socket one (no `peer`).
fn hba_method(
    engine: &Engine,
    peer: Option<SocketAddr>,
    local: bool,
    user: &str,
    database: &str,
    physical_repl: bool,
    default: AuthMethod,
) -> Result<(AuthMethod, u32, String)> {
    let Some(hba) = engine.server_conf().hba() else {
        return Ok((default, 1, format!("host all all all {}", default.name())));
    };
    let host = match peer {
        Some(p) => p.ip().to_canonical().to_string(),
        None => "[local]".to_string(),
    };
    let cat = engine.auth_catalog(database).ok();
    let member = |role: &str| {
        cat.as_ref().is_some_and(|c| match (c.role_by_name(user), c.role_by_name(role)) {
            (Some(u), Some(r)) => c.is_member_of_role(u.oid, r.oid, 0),
            _ => false,
        })
    };
    let refused = |what: &str| {
        let msg = if physical_repl {
            format!("{what} for replication connection from host \"{host}\", user \"{user}\", no encryption")
        } else {
            format!("{what} for host \"{host}\", user \"{user}\", database \"{database}\", no encryption")
        };
        Error::new(code::INVALID_AUTHORIZATION_SPECIFICATION, msg).fatal()
    };
    let Some(rule) = hba.choose(peer.map(|p| p.ip().to_canonical()), local, physical_repl, database, user, &member) else {
        return Err(refused("no pg_hba.conf entry"));
    };
    let method = match rule.method.as_str() {
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
    };
    Ok((method, rule.line, rule.raw.clone()))
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
    // Why a password check failed, for the server log (get_role_password's logdetail).
    why: &mut Option<String>,
) -> Result<()> {
    let cat = engine.auth_catalog(database)?;
    let role = cat.role_by_name(user).cloned();
    let now = crate::types::datetime::Clock::system().now;
    let shadow_detail = match &role {
        None => Some(format!("Role \"{user}\" does not exist.")),
        Some(r) if r.password.is_none() => Some(format!("User \"{user}\" has no password assigned.")),
        Some(r) if r.valid_until.is_some_and(|t| t < now) => Some(format!("User \"{user}\" has an expired password.")),
        Some(_) => None,
    };
    let failed = || {
        Error::new(
            code::INVALID_PASSWORD,
            format!("password authentication failed for user \"{user}\""),
        )
        .fatal()
    };
    match method {
        // (Whether the role exists and may log in is InitializeSessionUserId's, after
        // authentication: `check_role_login`.)
        AuthMethod::Trust => Ok(()),
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
                return authenticate(c, engine, user, database, AuthMethod::ScramSha256, why);
            };
            *why = shadow_detail.or_else(|| (!ok).then(|| format!("Password does not match for user \"{user}\".")));
            check_login(role.as_ref(), ok, failed)
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
            *why = match shadow_detail {
                None if !real => Some(format!("User \"{user}\" does not have a valid SCRAM secret.")),
                d => d,
            };
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
            if !real || expired {
                return Err(failed());
            }
            c.auth(12, server_final.as_bytes());
            Ok(())
        }
    }
}

/// After a password check: an expired password fails like a wrong one.
fn check_login(role: Option<&crate::catalog::Role>, ok: bool, failed: impl Fn() -> Error) -> Result<()> {
    let expired = role
        .and_then(|r| r.valid_until)
        .is_some_and(|t| t < crate::types::datetime::Clock::system().now);
    match role {
        Some(_) if ok && !expired => Ok(()),
        _ => Err(failed()),
    }
}

/// InitializeSessionUserId: the role must exist and may log in (after the authorization
/// line, before the database's checks).
fn check_role_login(engine: &Engine, user: &str, database: &str) -> Result<()> {
    let cat = engine.auth_catalog(database)?;
    let refused = |m: String| Error::new(code::INVALID_AUTHORIZATION_SPECIFICATION, m).fatal();
    match cat.role_by_name(user) {
        None => Err(refused(format!("role \"{user}\" does not exist"))),
        Some(r) if !r.login => Err(refused(format!("role \"{user}\" is not permitted to log in"))),
        Some(_) => Ok(()),
    }
}

fn expect_password_message(c: &mut Conn) -> Result<Vec<u8>> {
    match c.read_message(any_limit)? {
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

/// The backend's pg_stat_activity entry, found once per connection (`Engine::activity_slot`).
type Act = Option<crate::engine::ActivitySlot>;

fn set_activity(act: &Act, f: impl FnOnce(&mut crate::session::state::Activity)) {
    if let Some(a) = act {
        f(&mut a.lock());
    }
}

/// pg_stat_activity after a statement: the backend's state, and its application_name as
/// the statement left it (SET application_name reports itself: pgstat_report_appname).
fn statement_done_activity(act: &Act, s: &Session) {
    let settings = s.state.settings.borrow();
    let app = &settings.hot().application_name;
    let idle = s.status() == TxnStatus::Idle;
    let (xid, xmin) = s.activity_xids();
    set_activity(act, |a| {
        let state = activity_state(s);
        if a.state != state {
            a.state.clear();
            a.state.push_str(state);
            // (The clock read only when the state changes.)
            a.state_change = Some(crate::types::datetime::Clock::system().now);
        }
        if idle {
            a.xact_start = None;
        }
        a.backend_xid = xid;
        a.backend_xmin = xmin;
        if a.application_name != *app {
            a.application_name.clone_from(app);
        }
    });
}

/// pg_stat_activity as a statement (or a fastpath call) starts at `now`, its message's
/// statement timestamp (`Session::message_starts`): `state`, query_start, its
/// transaction's start (now, outside a transaction block), and the xmin a snapshot taken
/// now has.
fn start_activity(act: &Act, s: &Session, state: &str, sql: Option<&str>, now: i64) {
    // pgstat_report_activity keeps track_activity_query_size - 1 bytes of the text, cut at a
    // character boundary (pg_mbcliplen): pgAdmin's dashboard shows the clipped text.
    let sql = sql.map(|q| {
        let max = s.state.settings.borrow().hot().activity_query_size.saturating_sub(1);
        let mut n = q.len().min(max);
        while !q.is_char_boundary(n) {
            n -= 1;
        }
        &q[..n]
    });
    set_activity(act, |a| {
        if a.state != state {
            a.state.clear();
            a.state.push_str(state);
        }
        // (Every report's time, as pgstat_report_activity writes st_state_start_timestamp
        // whether the state changed or not.)
        a.state_change = Some(now);
        // (Copied only when it changed: a prepared statement's Bind and Execute name the same.)
        if let Some(q) = sql
            && a.query != q
        {
            a.query.clear();
            a.query.push_str(q);
        }
        a.query_start = Some(now);
        a.xact_start.get_or_insert(now);
        // (The xmin is looked up only when the entry has none: in a transaction, every
        // message but its first keeps it.)
        if a.backend_xmin.is_none() {
            a.backend_xmin = Some(s.state.db.xids().current_xmin());
        }
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
    // (Each message's body, read into the one allocation.)
    let mut raw: Vec<u8> = Vec::new();
    // The query of the last Parse, for what the server log says of errors after it.
    let mut last_stmt: Option<String> = None;
    let act = engine.activity_slot(pid);
    loop {
        c.log_sent(Some(s), pid, last_stmt.as_deref());
        let idle = if std::mem::take(&mut c.at_ready) { s.idle_timeout() } else { None };
        // (What the last message said is out, or parked until Sync: a CancelRequest waiting
        // on this backend may close its connection now.)
        if let Some(r) = &c.reads {
            r.fetch_add(1, Ordering::Release);
        }
        // (Whether this read waits for the client: nothing of the message is buffered.)
        let waits = c.rd.buffer().is_empty();
        // (An idle session keeps a small body allocation at most.)
        if raw.capacity() > 8192 {
            raw = Vec::new();
        }
        let msg = match c.read_message_into(idle.as_ref().map(|i| i.0), &mut raw, frontend_limit) {
            Ok(m) => m,
            Err(e) if e.get_ref().is_some_and(|e| e.is::<IdleTimeout>()) => {
                // The timeout's FATAL, then the session ends (its transaction rolls back).
                c.error(&idle.unwrap().1);
                return c.flush();
            }
            Err(e) if let Some(&UnexpectedType(t)) = e.get_ref().and_then(|e| e.downcast_ref()) => {
                // SocketBackend: garbage where a message should start, in any state (before
                // its length word is read).
                c.error(&Error::new(code::PROTOCOL_VIOLATION, format!("invalid frontend message type {t}")).fatal());
                return c.flush();
            }
            Err(e) => return Err(e),
        };
        let Some(t) = msg else {
            return Ok(());
        };
        s.notifications_busy();
        // A cancel that came while the session waited for this message cancels nothing
        // (ProcessInterrupts ignores QueryCancelPending while DoingCommandRead, which it
        // only sees if the read waits); one that came while it worked on the messages before
        // this one, already buffered, cancels the statement it runs next. (Cleared before
        // the check: a termination sets `terminated` before the cancel flag.)
        if waits {
            s.cancel.store(false, Ordering::Relaxed);
        }
        if terminated.load(Ordering::Relaxed) {
            return Ok(());
        }
        if skipping && t != b'S' && t != b'X' {
            continue;
        }
        sync_client_encoding(s);
        s.poll_config();
        // Query text arrives in the client's encoding.
        let converted;
        let body: &[u8] = match t {
            b'Q' | b'P' if !plain_utf8() => match convert_cstrings(&raw, 2) {
                Ok(b) => {
                    converted = b;
                    &converted
                }
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
            _ => &raw,
        };
        let r: Result<()> = match t {
            b'Q' => {
                let mut b = Body::new(body);
                // (Borrowed from the message's body: no copy per query.)
                let sql: &str = match b.cstr().and_then(|q| b.end().map(|()| q)) {
                    Ok(q) => q,
                    Err(e) => {
                        s.on_error();
                        c.error(&e);
                        send_changed(c, s);
                        ready_for_query(c, s)?;
                        continue;
                    }
                };
                start_activity(&act, s, "active", Some(sql), s.message_starts());
                s.start_xact_command();
                if let Some(mode) = c.repl
                    && (crate::repl::cmd::is_command(sql) || mode == crate::repl::walsender::Mode::Physical)
                {
                    let r = guard(c, s, |c, s| {
                        if !crate::repl::cmd::is_command(sql) {
                            return Err(Error::new(
                                code::FEATURE_NOT_SUPPORTED,
                                "cannot execute SQL commands in WAL sender for physical replication",
                            ));
                        }
                        let cmd = crate::repl::cmd::parse(sql)?;
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
                    match guard(c, s, |c, s| Ok(simple_query(c, s, sql))) {
                        Ok(r) => r?,
                        Err(e) => c.error(&e),
                    }
                }
                statement_done_activity(&act, s);
                c.log_sent(Some(s), pid, Some(sql));
                s.finish_xact_command();
                // (COPY FROM STDIN met a message it can't skip.)
                if c.sync_lost {
                    c.error(&sync_lost());
                }
                // A FATAL error (the session terminating itself) ends the session.
                if c.fatal {
                    return c.flush();
                }
                send_changed(c, s);
                ready_for_query(c, s)?;
                continue;
            }
            b'P' => {
                let mut b = Body::new(body);
                last_stmt = b.cstr().ok().and_then(|_| b.cstr().ok()).map(str::to_string);
                extended_activity(&act, s, t, body, s.message_starts());
                s.start_xact_command();
                guard(c, s, |c, s| parse_msg(c, s, body))
            }
            b'B' => {
                extended_activity(&act, s, t, body, s.message_starts());
                s.start_xact_command();
                guard(c, s, |c, s| bind_msg(c, s, body))
            }
            b'D' => {
                s.start_xact_command();
                guard(c, s, |c, s| describe_msg(c, s, body))
            }
            b'E' => {
                extended_activity(&act, s, t, body, s.message_starts());
                s.start_xact_command();
                guard(c, s, |c, s| execute_msg(c, s, body))
            }
            b'C' => guard(c, s, |c, s| close_msg(c, s, body)),
            b'S' => {
                skipping = false;
                if !body.is_empty() {
                    // (pq_getmsgend: the Sync isn't done; the error ends the transaction.)
                    s.on_error();
                    let _ = s.sync();
                    c.error(&Body::bad());
                } else if let Err(e) = guard(c, s, |_, s| s.sync()) {
                    send_notices(c, s);
                    c.error(&e);
                }
                send_notices(c, s);
                send_changed(c, s);
                statement_done_activity(&act, s);
                s.finish_xact_command();
                ready_for_query(c, s)?;
                continue;
            }
            b'H' if body.is_empty() => {
                c.flush()?;
                continue;
            }
            // (Flush's pq_getmsgend: an extended-query error, skipped to the Sync.)
            b'H' => Err(Body::bad()),
            b'X' => return Ok(()),
            // Copy messages outside COPY are ignored, as Postgres does.
            b'd' | b'c' | b'f' => continue,
            b'F' => {
                start_activity(&act, s, "fastpath function call", None, s.message_starts());
                let r = guard(c, s, |c, s| fastpath_msg(c, s, body));
                send_notices(c, s);
                if let Err(e) = r {
                    c.error(&e);
                }
                statement_done_activity(&act, s);
                send_changed(c, s);
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
            if c.sync_lost {
                c.error(&sync_lost());
            }
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
fn extended_activity(act: &Act, s: &Session, t: u8, body: &[u8], now: i64) {
    let mut b = Body::new(body);
    let sql = match t {
        b'P' => b.cstr().ok().and_then(|_| b.cstr().ok()),
        b'B' => b
            .cstr()
            .ok()
            .and_then(|_| b.cstr().ok())
            .and_then(|stmt| s.prepared.get(stmt).map(|p| p.sql.as_str())),
        _ => b
            .cstr()
            .ok()
            .and_then(|portal| s.portals.get(portal).map(|p| p.stmt.sql.as_str())),
    };
    start_activity(act, s, "active", sql, now);
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
    s.set_ps_idle();
    if s.status() == TxnStatus::Idle {
        for n in s.take_notifications() {
            notification(&mut c.out, &n);
        }
    }
    // (The statement's snapshot goes before the client hears the session is idle, as
    // Postgres releases it before ReadyForQuery: otherwise a client acting on the
    // ReadyForQuery, a DETACH ... FINALIZE's WaitForOlderSnapshots, could still see it.)
    s.note_idle();
    c.ready(s.status())?;
    s.notifications_idle();
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

/// ReportChangedGUCOptions, just before ReadyForQuery: the reported parameters whose value
/// changed (and differs from what the client was last told).
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
    if put_data_row_fixed(b, row, formats) {
        return Ok(());
    }
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
                // (array_send writes its element type: an array of a domain's is the
                // domain, where the value's type is the base type's array.)
                if let Some(elem) = cols
                    .get(i)
                    .and_then(|c| c.domain_array)
                    .and_then(|a| fmt.names.as_ref()?.array_domain_elem(a))
                    && b.len() >= at + 16
                {
                    b[at + 12..at + 16].copy_from_slice(&elem.to_be_bytes());
                }
            } else if !put_text_fast(b, v, ty) {
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

/// `put_data_row` of a row whose values are each null or one of fixed width sent in binary
/// (`binary::encode` writes those whatever the column's type), the message made on the stack
/// and copied in at once. False: another row (nothing written).
fn put_data_row_fixed(b: &mut BytesMut, row: &[crate::types::Value], formats: &[i16]) -> bool {
    use crate::types::Value as V;
    let mut m = [0u8; 256];
    let mut at = 7;
    for (i, v) in row.iter().enumerate() {
        let mut put = |x: &[u8]| {
            let end = at + 4 + x.len();
            if end > m.len() {
                return false;
            }
            m[at..at + 4].copy_from_slice(&(x.len() as i32).to_be_bytes());
            m[at + 4..end].copy_from_slice(x);
            at = end;
            true
        };
        if !matches!(v, V::Null) && result_format(formats, i) != 1 {
            return false;
        }
        let fits = match v {
            V::Null => {
                if at + 4 > m.len() {
                    return false;
                }
                m[at..at + 4].copy_from_slice(&(-1i32).to_be_bytes());
                at += 4;
                true
            }
            V::Bool(x) => put(&[*x as u8]),
            V::Int2(x) => put(&x.to_be_bytes()),
            V::Int4(x) => put(&x.to_be_bytes()),
            V::Int8(x) => put(&x.to_be_bytes()),
            V::Oid(x) => put(&x.to_be_bytes()),
            V::Xid8(x) => put(&x.to_be_bytes()),
            V::Float4(x) => put(&x.to_be_bytes()),
            V::Float8(x) => put(&x.to_be_bytes()),
            _ => false,
        };
        if !fits {
            return false;
        }
    }
    m[0] = b'D';
    m[1..5].copy_from_slice(&((at - 1) as i32).to_be_bytes());
    m[5..7].copy_from_slice(&(row.len() as i16).to_be_bytes());
    b.put_slice(&m[..at]);
    true
}

/// `put_data_row` of a row as its stored values (`data`, `storage::row`: the row is its
/// columns `picked`, ascending, of the result's columns `cols`), made from the stored bytes
/// without decoding them: each value null, of fixed width in binary (the bytes
/// `put_data_row_fixed` writes for the row decoded; a date, and a valid UTF-8 text or
/// varchar for a UTF-8 client, textsend's bytes), or in text one `put_text_fast` writes (a
/// bool, int2/4/8 or oid of its own type, a valid UTF-8 string of a string type for a UTF-8
/// client): the bytes `put_data_row` writes for the row decoded. False: another row (nothing
/// written).
fn put_data_row_stored(b: &mut BytesMut, data: &[u8], picked: &[u32], cols: &[OutCol], formats: &[i16]) -> Result<bool> {
    use crate::storage::row::tag;
    use crate::types::DataType as T;
    let mut m = [0u8; 256];
    let mut at = 7;
    let mut pos = 0;
    let stored = crate::storage::sst::get_varint(data, &mut pos)? as usize;
    let mut col = 0;
    for (i, &c) in picked.iter().enumerate() {
        let c = c as usize;
        // (A column added after the row was stored, or one before the last: decoded.)
        if c >= stored || c < col {
            return Ok(false);
        }
        while col < c {
            crate::storage::row::skip_value(data, &mut pos)?;
            col += 1;
        }
        let t = *data.get(pos).ok_or_else(|| crate::error::internal("truncated row"))?;
        let n = match t {
            tag::NULL => {
                if at + 4 > m.len() {
                    return Ok(false);
                }
                m[at..at + 4].copy_from_slice(&(-1i32).to_be_bytes());
                at += 4;
                pos += 1;
                col += 1;
                continue;
            }
            _ if result_format(formats, i) != 1 => {
                let ty = cols.get(i).map(|c| &c.ty);
                let x: i64 = match (t, ty) {
                    (tag::FALSE | tag::TRUE, Some(T::Bool)) => {
                        if at + 5 > m.len() {
                            return Ok(false);
                        }
                        m[at..at + 4].copy_from_slice(&1i32.to_be_bytes());
                        m[at + 4] = if t == tag::TRUE { b't' } else { b'f' };
                        at += 5;
                        pos += 1;
                        col += 1;
                        continue;
                    }
                    (tag::TEXT, Some(T::Text | T::Varchar(_) | T::Bpchar(_) | T::Name)) if plain_utf8() => {
                        let mut p = pos + 1;
                        let s = crate::storage::sst::get_varint(data, &mut p)? as usize;
                        let v = data.get(p..p + s).ok_or_else(|| crate::error::internal("truncated row"))?;
                        // (Invalid UTF-8 fails decoding the row: its error, from there.)
                        if at + 4 + s > m.len() || std::str::from_utf8(v).is_err() {
                            return Ok(false);
                        }
                        m[at..at + 4].copy_from_slice(&(s as i32).to_be_bytes());
                        m[at + 4..at + 4 + s].copy_from_slice(v);
                        at += 4 + s;
                        pos = p + s;
                        col += 1;
                        continue;
                    }
                    (tag::INT2, Some(T::Int2)) => i16::from_le_bytes(stored_le(data, pos)?) as i64,
                    (tag::INT4, Some(T::Int4)) => i32::from_le_bytes(stored_le(data, pos)?) as i64,
                    (tag::INT8, Some(T::Int8)) => i64::from_le_bytes(stored_le(data, pos)?),
                    (tag::OID, Some(T::Oid)) => u32::from_le_bytes(stored_le(data, pos)?) as i64,
                    _ => return Ok(false),
                };
                // (`put_text_fast`'s digits.)
                let mut buf = [0u8; 20];
                let mut d = buf.len();
                let mut u = x.unsigned_abs();
                loop {
                    d -= 1;
                    buf[d] = b'0' + (u % 10) as u8;
                    u /= 10;
                    if u == 0 {
                        break;
                    }
                }
                let n = buf.len() - d + usize::from(x < 0);
                if at + 4 + n > m.len() {
                    return Ok(false);
                }
                m[at..at + 4].copy_from_slice(&(n as i32).to_be_bytes());
                if x < 0 {
                    m[at + 4] = b'-';
                }
                m[at + 4 + n - (buf.len() - d)..at + 4 + n].copy_from_slice(&buf[d..]);
                at += 4 + n;
                pos += 1 + match t {
                    tag::INT2 => 2,
                    tag::INT8 => 8,
                    _ => 4,
                };
                col += 1;
                continue;
            }
            tag::TEXT if matches!(cols.get(i).map(|c| &c.ty), Some(T::Text | T::Varchar(_))) && plain_utf8() => {
                let mut p = pos + 1;
                let n = crate::storage::sst::get_varint(data, &mut p)? as usize;
                let v = data.get(p..p + n).ok_or_else(|| crate::error::internal("truncated row"))?;
                // (Invalid UTF-8 fails decoding the row: its error, from there.)
                if at + 4 + n > m.len() || std::str::from_utf8(v).is_err() {
                    return Ok(false);
                }
                m[at..at + 4].copy_from_slice(&(n as i32).to_be_bytes());
                m[at + 4..at + 4 + n].copy_from_slice(v);
                at += 4 + n;
                pos = p + n;
                col += 1;
                continue;
            }
            tag::DATE if matches!(cols.get(i).map(|c| &c.ty), Some(T::Date)) => 4,
            tag::FALSE | tag::TRUE => 0,
            tag::INT2 => 2,
            tag::INT4 | tag::FLOAT4 | tag::OID => 4,
            tag::INT8 | tag::FLOAT8 => 8,
            _ => return Ok(false),
        };
        let end = at + 4 + n.max(1);
        if end > m.len() {
            return Ok(false);
        }
        let v = data.get(pos + 1..pos + 1 + n).ok_or_else(|| crate::error::internal("truncated row"))?;
        m[at..at + 4].copy_from_slice(&(n.max(1) as i32).to_be_bytes());
        if n == 0 {
            m[at + 4] = u8::from(t == tag::TRUE);
        } else {
            // (Stored little-endian, sent big-endian.)
            for (d, s) in m[at + 4..end].iter_mut().zip(v.iter().rev()) {
                *d = *s;
            }
        }
        at = end;
        pos += 1 + n;
        col += 1;
    }
    m[0] = b'D';
    m[1..5].copy_from_slice(&((at - 1) as i32).to_be_bytes());
    m[5..7].copy_from_slice(&(picked.len() as i16).to_be_bytes());
    b.put_slice(&m[..at]);
    Ok(true)
}

/// The `N` stored bytes of the fixed-width value whose tag is at `pos`.
fn stored_le<const N: usize>(data: &[u8], pos: usize) -> Result<[u8; N]> {
    data.get(pos + 1..pos + 1 + N)
        .and_then(|v| v.try_into().ok())
        .ok_or_else(|| crate::error::internal("truncated row"))
}

/// The text output of the commonest values written straight into the message (what
/// `format_value` would make, without a String per value): text, integers, booleans and
/// plain oids for a UTF-8 client, numerics for any. False: not one of those.
fn put_text_fast(b: &mut BytesMut, v: &crate::types::Value, ty: &crate::types::DataType) -> bool {
    use crate::types::{DataType as T, Value as V};
    let n = match (v, ty) {
        (V::Text(s), T::Text | T::Varchar(_) | T::Bpchar(_) | T::Name) => {
            if !plain_utf8() {
                return false;
            }
            b.put_slice(s.as_bytes());
            return true;
        }
        (V::Bool(x), T::Bool) => {
            b.put_u8(if *x { b't' } else { b'f' });
            return true;
        }
        (V::Int2(i), T::Int2) => *i as i64,
        (V::Int4(i), T::Int4) => *i as i64,
        (V::Int8(i), T::Int8) => *i,
        (V::Oid(o), T::Oid) => *o as i64,
        // (Numeric's digits are written without allocating: `Numeric`'s Display.)
        (V::Numeric(x), T::Numeric(_)) => {
            use std::fmt::Write;
            return write!(b, "{x}").is_ok();
        }
        _ => return false,
    };
    // (Digits into a stack buffer from the end; the client's encoding keeps ASCII as is.)
    let mut buf = [0u8; 20];
    let mut at = buf.len();
    let mut u = n.unsigned_abs();
    loop {
        at -= 1;
        buf[at] = b'0' + (u % 10) as u8;
        u /= 10;
        if u == 0 {
            break;
        }
    }
    if n < 0 {
        b.put_u8(b'-');
    }
    b.put_slice(&buf[at..]);
    true
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
    wr: Arc<TcpStream>,
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

    fn row_stored(&mut self, data: &[u8], picked: &[u32], cols: &[OutCol]) -> Result<bool> {
        let formats = std::mem::take(&mut self.formats);
        let mut sent = false;
        let r = self.put(|b| {
            sent = put_data_row_stored(b, data, picked, cols, &formats)?;
            Ok(())
        });
        self.formats = formats;
        r.map(|()| sent)
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
    streamed_into(c, s, Box::new(WireSink { wr: c.wr.clone(), formats: formats.to_vec(), describe }), f)
}

/// `streamed`, the rows into `sink`.
fn streamed_into<T>(
    c: &mut Conn,
    s: &mut Session,
    sink: Box<dyn crate::session::RowSink>,
    f: impl FnOnce(&mut Session) -> T,
) -> T {
    s.row_sink = Some(sink);
    let r = c.unflushed(|| f(s));
    s.row_sink = None;
    r
}

/// printtup_prepare_info's error for a result format code that's neither text nor binary.
fn unsupported_format(f: i16) -> Error {
    Error::new(code::INVALID_PARAMETER_VALUE, format!("unsupported format code: {f}")).routine("printtup_prepare_info")
}

/// The rows of a portal Bound with a result format code that's neither text nor binary:
/// the first row that has columns fails (`unsupported_format`), before any of it goes out.
/// A statement that sends no rows (or COPY's data) runs as it would.
struct BadFormat(WireSink, i16);

impl crate::session::RowSink for BadFormat {
    fn begin(&mut self, cols: &[OutCol]) -> Result<()> {
        self.0.begin(cols)
    }

    fn row(&mut self, row: crate::exec::Row, cols: &[OutCol], fmt: &FormatCtx) -> Result<()> {
        if row.is_empty() {
            return self.0.row(row, cols, fmt);
        }
        Err(unsupported_format(self.1))
    }

    fn row_stored(&mut self, data: &[u8], picked: &[u32], cols: &[OutCol]) -> Result<bool> {
        if picked.is_empty() {
            return self.0.row_stored(data, picked, cols);
        }
        Err(unsupported_format(self.1))
    }

    fn takes_copy(&self) -> bool {
        self.0.takes_copy()
    }

    fn copy_begin(&mut self, binary: bool, ncols: usize) -> Result<()> {
        self.0.copy_begin(binary, ncols)
    }

    fn copy_data(&mut self, data: &[u8]) -> Result<()> {
        self.0.copy_data(data)
    }

    fn copy_end(&mut self) -> Result<()> {
        self.0.copy_end()
    }
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
    if stmts.is_empty() || stmts.iter().all(|p| p.stmt.shape().is_none()) {
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
    let errors = c.errors;
    let r = simple_batch(c, s, sql, &stmts);
    s.set_batch_len(0);
    // (finish_xact_command comes before the duration line.)
    s.finish_xact_command();
    // check_log_duration (not for a statement that failed).
    if c.errors == errors
        && let Some((ms, with_stmt)) = s.duration_logged(logged)
    {
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
        s.set_ps(p);
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
            // (CopyGetData: a message of another type, or one whose length is out of range,
            // fails the COPY in the middle of reading it: the connection can't go on.)
            let lost = std::cell::Cell::new(false);
            let mut read = || -> Result<Option<Vec<u8>>> {
                loop {
                    let m = c.read_message(copy_in_limit).map_err(|e| {
                        lost.set(true);
                        match e.get_ref().and_then(|e| e.downcast_ref::<UnexpectedType>()) {
                            Some(&UnexpectedType(t)) => Error::new(
                                code::PROTOCOL_VIOLATION,
                                format!("unexpected message type 0x{t:02X} during COPY from stdin"),
                            ),
                            None => Error::new(
                                code::CONNECTION_FAILURE,
                                "unexpected EOF on client connection with an open transaction",
                            )
                            .routine("CopyGetData"),
                        }
                    })?;
                    match m {
                        Some((b'd', body)) => return Ok(Some(body)),
                        Some((b'c', _)) => return Ok(None),
                        Some((b'f', body)) => {
                            let msg = Body::new(&body).cstr()?.to_string();
                            return Err(Error::new(
                                code::QUERY_CANCELED,
                                format!("COPY from stdin failed: {msg}"),
                            ));
                        }
                        Some(_) => {}
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
            c.sync_lost |= lost.get();
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
    b.end()?;
    if name.is_empty() {
        s.close_statement("");
    }
    s.parse_message(&name, &sql, &oids)
        .map_err(|e| e.in_chars(&sql))?;
    // exec_parse_message's duration line.
    if let Some((ms, with_stmt)) = s.duration_logged(false) {
        let what = if name.is_empty() { "<unnamed>" } else { name.as_str() };
        let text = if with_stmt { format!("duration: {ms} ms  parse {what}: {sql}") } else { format!("duration: {ms} ms") };
        s.log_message(&text, None);
    }
    send_notices(c, s);
    c.msg(b'1', |_| {});
    Ok(())
}

fn bind_msg(c: &mut Conn, s: &mut Session, body: &[u8]) -> Result<()> {
    // (The names, formats and parameters read in place from the message's body: an
    // allocation each was a third of a prepared statement's.)
    let mut b = Body::new(body);
    let portal = b.cstr()?;
    let stmt = b.cstr()?;
    let nf = b.i16()? as u16 as usize;
    let mut formats: smallvec::SmallVec<[i16; 8]> = smallvec::SmallVec::with_capacity(nf);
    for _ in 0..nf {
        formats.push(b.i16()?);
    }
    let np = b.i16()? as u16 as usize;
    let mut params: smallvec::SmallVec<[Option<&[u8]>; 8]> = smallvec::SmallVec::with_capacity(np);
    for _ in 0..np {
        let len = b.i32()?;
        params.push(if len < 0 { None } else { Some(b.bytes(len as usize)?) });
    }
    let nr = b.i16()? as u16 as usize;
    let mut result_formats = Vec::with_capacity(nr);
    for _ in 0..nr {
        result_formats.push(b.i16()?);
    }
    // (What's left over fails the Bind after its parameters are read: `trailing`.)
    s.bind_message_in(portal, stmt, &params, &formats, result_formats, b.end().is_err())?;
    // exec_bind_message's duration line (with the parameters, as errdetail_params).
    if let Some((ms, with_stmt)) = s.duration_logged(false) {
        if with_stmt && let Some((what, detail, p)) = s.portal_log(portal) {
            s.log_message(&format!("duration: {ms} ms  bind {what}: {}", p.sql), detail.as_deref());
        } else {
            s.log_message(&format!("duration: {ms} ms"), None);
        }
    }
    send_notices(c, s);
    c.msg(b'2', |_| {});
    Ok(())
}

/// IsAbortedTransactionBlockState's refusal, raised by `routine`.
fn aborted_txn(routine: &'static str) -> Error {
    Error::new(
        code::IN_FAILED_SQL_TRANSACTION,
        "current transaction is aborted, commands ignored until end of transaction block",
    )
    .routine(routine)
}

fn describe_msg(c: &mut Conn, s: &mut Session, body: &[u8]) -> Result<()> {
    let mut b = Body::new(body);
    let kind = b.u8()?;
    let name = b.cstr()?.to_string();
    b.end()?;
    match kind {
        b'S' => {
            let Some(p) = s.prepared.get(&name).cloned() else {
                return Err(crate::session::missing_statement(&name, "exec_describe_statement_message"));
            };
            let cols = s.describe(&p)?;
            // (A statement returning rows isn't described in a failed transaction.)
            if s.status() == TxnStatus::Failed && cols.is_some() {
                return Err(aborted_txn("exec_describe_statement_message"));
            }
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
                ).routine("exec_describe_portal_message"));
            };
            if s.status() == TxnStatus::Failed && cols.is_some() {
                return Err(aborted_txn("exec_describe_portal_message"));
            }
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
    b.end()?;
    // (The statement held, for its text: not a copy of it per Execute.)
    let (formats, stmt, empty, bad_format) = match s.portals.get(&portal) {
        Some(p) => (p.result_formats.clone(), Some(p.stmt.clone()), p.stmt.stmt.is_none(), p.bad_format),
        None => (Vec::new(), None, false, None),
    };
    let sql = stmt.as_ref().map_or("", |p| p.sql.as_str());
    if empty {
        c.msg(b'I', |_| {});
        return Ok(());
    }
    send_notices(c, s);
    // exec_execute_message's log_statement and duration lines.
    s.set_ps_portal(&portal);
    let info = if s.state.settings.borrow().logs_statements() { s.portal_log(&portal) } else { None };
    let logged = info.as_ref().is_some_and(|(_, _, p)| s.statement_logged(std::slice::from_ref(p)));
    if let Some((what, detail, _)) = info.as_ref().filter(|_| logged) {
        s.log_message(&format!("execute {what}: {sql}"), detail.as_deref());
        send_notices(c, s);
    }
    let r = match bad_format {
        None => streamed(c, s, &formats, false, |s| s.execute_portal(&portal, max_rows)),
        Some(f) => {
            let sink = BadFormat(WireSink { wr: c.wr.clone(), formats: formats.clone(), describe: false }, f);
            streamed_into(c, s, Box::new(sink), |s| s.execute_portal(&portal, max_rows))
        }
    };
    if r.is_ok() {
        s.note_executed();
        if let Some((what, detail, _)) = &info
            && let Some((ms, with_stmt)) = s.duration_logged(logged)
        {
            if with_stmt {
                s.log_message(&format!("duration: {ms} ms  execute {what}: {sql}"), detail.as_deref());
            } else {
                s.log_message(&format!("duration: {ms} ms"), None);
            }
        }
    }
    send_notices(c, s);
    let (o, more) = r.map_err(|e| e.in_chars(sql))?;
    if let (Some(f), Outcome::Done(r)) = (bad_format, &o)
        && r.rows.iter().any(|row| !row.is_empty())
    {
        return Err(unsupported_format(f));
    }
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
    b.end()?;
    match kind {
        b'S' => s.close_statement(&name),
        b'P' => s.close_portal(&name)?,
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
    fn idle_read_parks_after_trimming_and_rearms_on_data() {
        use super::{IDLE_TRIM, IdleRead, IdleTimeout, set_idle_hook};
        use std::io::{Read, Write};
        use std::net::{TcpListener, TcpStream};
        use std::time::{Duration, Instant};

        set_idle_hook(|| {});
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let socket = listener.accept().unwrap().0;
        let observer = socket.try_clone().unwrap();
        let mut reader = IdleRead::new(std::sync::Arc::new(socket)).unwrap();
        let worker = std::thread::spawn(move || {
            let mut byte = [0];
            reader.read_exact(&mut byte).unwrap();
            (reader, byte)
        });
        let deadline = Instant::now() + Duration::from_secs(6);
        while observer.read_timeout().unwrap().is_some() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let parked = observer.read_timeout().unwrap().is_none();
        client.write_all(&[42]).unwrap();
        let (mut reader, byte) = worker.join().unwrap();
        assert!(parked, "a trimmed idle connection still has a polling timeout");
        assert_eq!(byte, [42]);
        assert_eq!(observer.read_timeout().unwrap(), Some(IDLE_TRIM));

        reader.deadline = Some(Instant::now() + IDLE_TRIM + Duration::from_millis(50));
        let error = reader.read(&mut [0]).unwrap_err();
        assert!(error.get_ref().unwrap().is::<IdleTimeout>());
    }

    #[test]
    fn wait_spin_polls_only_for_a_quick_client() {
        use super::{IdleRead, set_wait_spin};
        use std::io::{Read, Write};
        use std::net::{TcpListener, TcpStream};
        use std::time::Duration;

        // (5 ms, not performance mode's 50 µs: a loaded machine can preempt a read for longer.)
        set_wait_spin(Duration::from_millis(5));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let mut reader = IdleRead::new(std::sync::Arc::new(listener.accept().unwrap().0)).unwrap();
        let mut b = [0u8; 1];
        client.write_all(&[1]).unwrap();
        reader.read_exact(&mut b).unwrap();
        assert_eq!(b, [1]);
        // A reply past the poll's time: read when it comes, and no poll before the next.
        let late = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            client.write_all(&[2]).unwrap();
            client
        });
        reader.read_exact(&mut b).unwrap();
        let mut client = late.join().unwrap();
        assert_eq!((b, reader.spin), ([2], false));
        // A quick one again: polls again.
        client.write_all(&[3]).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        reader.read_exact(&mut b).unwrap();
        set_wait_spin(Duration::ZERO);
        assert_eq!((b, reader.spin), ([3], true));
    }

    #[test]
    fn startup_options() {
        assert_eq!(
            parse_options(
                "-c search_path=app,public --statement-timeout=5s -cwork_mem=4MB -c a=b\\ c"
            )
            .0,
            vec![
                ("search_path".into(), "app,public".into()),
                ("statement_timeout".into(), "5s".into()),
                ("work_mem".into(), "4MB".into()),
                ("a".into(), "b c".into()),
            ]
        );
        // Switches, and what stops them (process_postgres_switches).
        let (set, e) = parse_options("-S 2000 -e -fs -q");
        let set: Vec<String> = set.into_iter().map(|(k, v)| format!("{k}={v}")).collect();
        assert_eq!(set, ["work_mem=2000", "datestyle=euro", "enable_seqscan=false"]);
        assert_eq!(e.unwrap().message, "invalid command-line argument for server process: -q");
        assert_eq!(parse_options("-c work_mem").1.unwrap().message, "-c work_mem requires a value");
        assert_eq!(parse_options("x -S 1").1.unwrap().message, "invalid command-line argument for server process: x");
    }
}

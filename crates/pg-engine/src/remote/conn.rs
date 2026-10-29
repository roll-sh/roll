//! A blocking PostgreSQL client connection (what libpq's `PGconn` is to dblink and
//! postgres_fdw): startup and authentication (trust, cleartext, md5, SCRAM-SHA-256), simple
//! and extended queries with text results, asynchronous sends, notifications and cancel
//! requests. Built on `postgres-protocol`'s message codecs.

use super::conninfo::{self, ConnInfo};
use super::outbound::Outbound;
use bytes::BytesMut;
use postgres_protocol::message::{backend, frontend};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

/// Where connections may go, and how the engine itself is reached.
#[derive(Clone, Debug, Default)]
pub struct Env {
    /// The operator's allow-list (None: every outbound connection refused).
    pub outbound: Option<Outbound>,
    /// The engine's own listener (what a connection without a host reaches).
    pub local: Option<SocketAddr>,
    /// The port the engine reports (`port`): a connection without a host reaches the engine
    /// at this port only, as a Unix socket is per port.
    pub local_port: u16,
    /// The default user name (libpq's: the operating system user the server runs as, here
    /// the bootstrap superuser).
    pub default_user: String,
}

/// libpq's DEFAULT_PGSOCKET_DIR (the reference build's).
pub const SOCKET_DIR: &str = "/var/run/postgresql";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    EmptyQuery,
    CommandOk,
    TuplesOk,
    CopyOut,
    CopyIn,
    FatalError,
}

#[derive(Clone, Debug)]
pub struct Field {
    pub name: String,
    pub type_oid: u32,
    pub typmod: i32,
}

/// A remote error or notice's fields (PQresultErrorField), by field code.
#[derive(Clone, Debug, Default)]
pub struct ErrorFields(pub Vec<(u8, String)>);

impl ErrorFields {
    pub fn get(&self, code: u8) -> Option<&str> {
        self.0.iter().find(|(c, _)| *c == code).map(|(_, v)| v.as_str())
    }
    pub fn severity(&self) -> &str {
        self.get(b'S').unwrap_or("ERROR")
    }
    pub fn sqlstate(&self) -> Option<&str> {
        self.get(b'C')
    }
    pub fn message(&self) -> Option<&str> {
        self.get(b'M')
    }
    pub fn detail(&self) -> Option<&str> {
        self.get(b'D')
    }
    pub fn hint(&self) -> Option<&str> {
        self.get(b'H')
    }
    pub fn context(&self) -> Option<&str> {
        self.get(b'W')
    }

    /// pqBuildErrorMessage3 at the default verbosity: "SEVERITY:  message\nDETAIL:  ...\n".
    pub fn text(&self) -> String {
        let mut s = format!("{}:  {}\n", self.severity(), self.message().unwrap_or(""));
        if let Some(d) = self.detail() {
            s.push_str(&format!("DETAIL:  {d}\n"));
        }
        if let Some(h) = self.hint() {
            s.push_str(&format!("HINT:  {h}\n"));
        }
        s
    }
}

/// A query's result (PGresult), values in text format.
#[derive(Clone, Debug)]
pub struct PgResult {
    pub status: Status,
    pub fields: Vec<Field>,
    pub rows: Vec<Vec<Option<String>>>,
    /// The command tag (PQcmdStatus).
    pub cmd_status: String,
    pub error: Option<ErrorFields>,
}

impl PgResult {
    fn new(status: Status) -> PgResult {
        PgResult {
            status,
            fields: Vec::new(),
            rows: Vec::new(),
            cmd_status: String::new(),
            error: None,
        }
    }

    pub fn ok(&self) -> bool {
        matches!(self.status, Status::CommandOk | Status::TuplesOk)
    }

    /// PQcmdTuples: the row count of an INSERT / UPDATE / DELETE / ... tag.
    pub fn cmd_tuples(&self) -> u64 {
        self.cmd_status.rsplit(' ').next().and_then(|n| n.parse().ok()).unwrap_or(0)
    }
}

/// An asynchronous notification (PGnotify).
#[derive(Clone, Debug)]
pub struct Notify {
    pub channel: String,
    pub pid: i32,
    pub payload: String,
}

/// PQtransactionStatus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxStatus {
    Idle,
    Active,
    InTrans,
    InError,
    Unknown,
}

/// What reads check while they wait (CHECK_FOR_INTERRUPTS): an error ends the wait (the
/// query in progress is cancelled first).
pub type Interrupt<'a> = &'a dyn Fn() -> crate::Result<()>;

pub struct Conn {
    stream: TcpStream,
    addr: SocketAddr,
    rbuf: BytesMut,
    /// ParameterStatus values (PQparameterStatus).
    pub params: HashMap<String, String>,
    pub pid: i32,
    secret: i32,
    tx: TxStatus,
    /// A query was sent and its ReadyForQuery hasn't been read.
    busy: bool,
    /// Results read and not taken yet; the one being read.
    results: VecDeque<PgResult>,
    cur: Option<PgResult>,
    /// ReadyForQuery read while `busy` (get_result then returns None).
    ready: bool,
    notifies: VecDeque<Notify>,
    /// The server asked for a password and got one (PQconnectionUsedPassword).
    pub used_password: bool,
    /// PQerrorMessage: the last error's text.
    pub error_message: String,
    /// The connection broke (CONNECTION_BAD).
    pub bad: bool,
    /// The options it was made with (user, dbname, host, port as used).
    pub info: ConnInfo,
}

/// Connect as `info` says (PQconnectdb). The error is libpq's PQerrorMessage, chomped.
pub fn connect(info: &ConnInfo, env: &Env) -> Result<Conn, String> {
    let port_s = conninfo::effective(info, "port").unwrap_or_else(|| "5432".into());
    let hosts: Vec<String> = info
        .nonempty("host")
        .map_or_else(|| vec![String::new()], |h| h.split(',').map(str::to_string).collect());
    let hostaddrs: Vec<String> = info
        .nonempty("hostaddr")
        .map_or_else(Vec::new, |h| h.split(',').map(str::to_string).collect());
    let ports: Vec<&str> = port_s.split(',').collect();
    let n = hosts.len().max(hostaddrs.len());
    if ports.len() != 1 && ports.len() != n {
        return Err(format!("could not match {} port numbers to {} hosts", ports.len(), n));
    }
    let timeout = conninfo::effective(info, "connect_timeout")
        .and_then(|t| t.trim().parse::<u64>().ok())
        .filter(|t| *t > 0)
        .map(|t| Duration::from_secs(t.max(2)));
    let mut errors = String::new();
    for i in 0..n {
        let host = hosts.get(i).cloned().unwrap_or_default();
        let hostaddr = hostaddrs.get(i).cloned().unwrap_or_default();
        let port_str = ports.get(i).or(ports.first()).copied().unwrap_or("5432");
        let port: u16 = match port_str.trim().parse::<u32>() {
            Ok(p) if (1..=65535).contains(&p) => p as u16,
            _ => {
                errors.push_str(&format!("invalid port number: \"{port_str}\"\n"));
                continue;
            }
        };
        match connect_one(info, env, &host, &hostaddr, port, timeout) {
            Ok(c) => return Ok(c),
            Err(e) => errors.push_str(&e),
        }
    }
    Err(errors.trim_end_matches('\n').to_string())
}

/// What a refused destination reports, after libpq's "connection to server ... failed: ".
const REFUSED: &str = "outbound connections to this destination are not allowed by this server\n\
    \tThe server operator can allow destinations with roll-engine --allow-outbound.\n";

/// The address a connection to `host` / `hostaddr` at `port` goes to, and libpq's prefix
/// for its failures (emitHostIdentityInfo); refused when the allow-list doesn't have it.
fn destination(env: &Env, host: &str, hostaddr: &str, port: u16) -> Result<(SocketAddr, String), String> {
    if hostaddr.is_empty() && (host.is_empty() || host.starts_with('/')) {
        let dir = if host.is_empty() { SOCKET_DIR } else { host };
        let prefix = format!("connection to server on socket \"{dir}/.s.PGSQL.{port}\" failed: ");
        if !env.outbound.as_ref().is_some_and(Outbound::allows_local) {
            return Err(format!("{prefix}{REFUSED}"));
        }
        return match env.local {
            Some(a) if port == env.local_port && dir == SOCKET_DIR => {
                let ip = match a.ip() {
                    ip if !ip.is_unspecified() => ip,
                    IpAddr::V4(_) => IpAddr::from([127, 0, 0, 1]),
                    IpAddr::V6(_) => IpAddr::from([0, 0, 0, 0, 0, 0, 0, 1u16]),
                };
                Ok((SocketAddr::new(ip, a.port()), prefix))
            }
            _ => Err(format!(
                "{prefix}No such file or directory\n\tIs the server running locally and accepting connections on that socket?\n"
            )),
        };
    }
    let unknown = |h: &str| format!("could not translate host name \"{h}\" to address: Name or service not known\n");
    let addrs: Vec<SocketAddr> = if !hostaddr.is_empty() {
        match hostaddr.parse::<IpAddr>() {
            Ok(ip) => vec![SocketAddr::new(ip, port)],
            Err(_) => return Err(format!("could not parse network address \"{hostaddr}\": Name or service not known\n")),
        }
    } else {
        (host, port).to_socket_addrs().map_err(|_| unknown(host))?.collect()
    };
    let Some(first) = addrs.first().copied() else {
        return Err(unknown(host));
    };
    let shown = if hostaddr.is_empty() { host } else { hostaddr };
    let prefix = if hostaddr.is_empty() && first.ip().to_string() != host {
        format!("connection to server at \"{shown}\" ({}), port {port} failed: ", first.ip())
    } else {
        format!("connection to server at \"{shown}\", port {port} failed: ")
    };
    let ips: Vec<IpAddr> = addrs.iter().map(|a| a.ip()).collect();
    let allowed = env.outbound.as_ref().is_some_and(|o| {
        o.allows(host, &ips, port) || (!hostaddr.is_empty() && o.allows(hostaddr, &ips, port))
    });
    if !allowed {
        return Err(format!("{prefix}{REFUSED}"));
    }
    Ok((first, prefix))
}

fn connect_one(info: &ConnInfo, env: &Env, host: &str, hostaddr: &str, port: u16, timeout: Option<Duration>) -> Result<Conn, String> {
    let (addr, prefix) = destination(env, host, hostaddr, port)?;
    let stream = match timeout {
        Some(t) => TcpStream::connect_timeout(&addr, t),
        None => TcpStream::connect(addr),
    };
    let stream = stream.map_err(|e| {
        format!("{prefix}{}\n\tIs the server running on that host and accepting TCP/IP connections?\n", io_text(&e))
    })?;
    let _ = stream.set_nodelay(true);
    // (The engine's own "Unix socket": pg_hba.conf's local lines apply to it.)
    if hostaddr.is_empty() && (host.is_empty() || host.starts_with('/'))
        && let Ok(a) = stream.local_addr()
    {
        crate::hba::mark_local(a);
    }
    let mut c = Conn {
        stream,
        addr,
        rbuf: BytesMut::with_capacity(8192),
        params: HashMap::new(),
        pid: 0,
        secret: 0,
        tx: TxStatus::Idle,
        busy: false,
        results: VecDeque::new(),
        cur: None,
        ready: false,
        notifies: VecDeque::new(),
        used_password: false,
        error_message: String::new(),
        bad: false,
        info: info.clone(),
    };
    let user = conninfo::effective(info, "user").unwrap_or_else(|| env.default_user.clone());
    let dbname = conninfo::effective(info, "dbname").unwrap_or_else(|| user.clone());
    c.info.set("user", &user);
    c.info.set("dbname", &dbname);
    c.info.set("port", &port.to_string());
    let deadline = timeout.map(|t| Instant::now() + t);
    c.startup(info, &user, &dbname, deadline).map_err(|e| format!("{prefix}{e}"))?;
    Ok(c)
}

/// strerror text of a connect failure.
fn io_text(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::ConnectionRefused => "Connection refused".into(),
        std::io::ErrorKind::TimedOut => "Connection timed out".into(),
        std::io::ErrorKind::HostUnreachable => "No route to host".into(),
        std::io::ErrorKind::NetworkUnreachable => "Network is unreachable".into(),
        _ => e.to_string(),
    }
}

impl Conn {
    /// The startup packet, authentication, and the parameters up to ReadyForQuery. The
    /// error is the text after libpq's "connection to server ... failed: ".
    fn startup(&mut self, info: &ConnInfo, user: &str, dbname: &str, deadline: Option<Instant>) -> Result<(), String> {
        let mut params: Vec<(&str, String)> = vec![("user", user.to_string()), ("database", dbname.to_string())];
        if let Some(a) = info.get("application_name").or(info.get("fallback_application_name")) {
            params.push(("application_name", a.to_string()));
        }
        if let Some(o) = info.nonempty("options") {
            params.push(("options", o.to_string()));
        }
        if let Some(e) = info.nonempty("client_encoding") {
            params.push(("client_encoding", e.to_string()));
        }
        // A walsender connection (`replication=database`, or `true` for physical).
        if let Some(r) = info.nonempty("replication") {
            params.push(("replication", r.to_string()));
        }
        let mut buf = BytesMut::new();
        frontend::startup_message(params.iter().map(|(k, v)| (*k, v.as_str())), &mut buf).map_err(|e| e.to_string())?;
        self.send(&buf).map_err(|e| format!("{e}\n"))?;
        let password = info.nonempty("password").map(str::to_string);
        let mut scram: Option<postgres_protocol::authentication::sasl::ScramSha256> = None;
        loop {
            let m = self.read_message(deadline).map_err(|e| format!("{e}\n"))?;
            let mut out = BytesMut::new();
            match m {
                backend::Message::AuthenticationOk => {}
                backend::Message::AuthenticationCleartextPassword => {
                    let Some(p) = &password else {
                        return Err("fe_sendauth: no password supplied\n".into());
                    };
                    frontend::password_message(p.as_bytes(), &mut out).map_err(|e| e.to_string())?;
                    self.used_password = true;
                }
                backend::Message::AuthenticationMd5Password(b) => {
                    let Some(p) = &password else {
                        return Err("fe_sendauth: no password supplied\n".into());
                    };
                    let h = postgres_protocol::authentication::md5_hash(user.as_bytes(), p.as_bytes(), b.salt());
                    frontend::password_message(h.as_bytes(), &mut out).map_err(|e| e.to_string())?;
                    self.used_password = true;
                }
                backend::Message::AuthenticationSasl(b) => {
                    let mut mechs = b.mechanisms();
                    let mut ok = false;
                    while let Ok(Some(m)) = fallible_iterator::FallibleIterator::next(&mut mechs) {
                        ok |= m == postgres_protocol::authentication::sasl::SCRAM_SHA_256;
                    }
                    if !ok {
                        return Err("none of the server's SASL authentication mechanisms are supported\n".into());
                    }
                    let Some(p) = &password else {
                        return Err("fe_sendauth: no password supplied\n".into());
                    };
                    let s = postgres_protocol::authentication::sasl::ScramSha256::new(
                        p.as_bytes(),
                        postgres_protocol::authentication::sasl::ChannelBinding::unsupported(),
                    );
                    frontend::sasl_initial_response(postgres_protocol::authentication::sasl::SCRAM_SHA_256, s.message(), &mut out)
                        .map_err(|e| e.to_string())?;
                    scram = Some(s);
                    self.used_password = true;
                }
                backend::Message::AuthenticationSaslContinue(b) => {
                    let s = scram.as_mut().ok_or("fe_sendauth: invalid authentication request from server: AUTH_REQ_SASL_CONT without AUTH_REQ_SASL\n")?;
                    s.update(b.data()).map_err(|_| "fe_sendauth: error in SASL authentication\n".to_string())?;
                    frontend::sasl_response(s.message(), &mut out).map_err(|e| e.to_string())?;
                }
                backend::Message::AuthenticationSaslFinal(b) => {
                    let s = scram.as_mut().ok_or("fe_sendauth: error in SASL authentication\n")?;
                    s.finish(b.data()).map_err(|_| "fe_sendauth: error in SASL authentication\n".to_string())?;
                }
                backend::Message::AuthenticationGss
                | backend::Message::AuthenticationKerberosV5
                | backend::Message::AuthenticationScmCredential
                | backend::Message::AuthenticationSspi
                | backend::Message::AuthenticationGssContinue(_) => {
                    return Err("authentication method requested by server is not supported\n".into());
                }
                backend::Message::BackendKeyData(k) => {
                    self.pid = k.process_id();
                    self.secret = k.secret_key();
                }
                backend::Message::ParameterStatus(p) => {
                    if let (Ok(n), Ok(v)) = (p.name(), p.value()) {
                        self.params.insert(n.to_string(), v.to_string());
                    }
                }
                backend::Message::NoticeResponse(_) => {}
                backend::Message::ErrorResponse(e) => return Err(error_fields(e.fields()).text()),
                backend::Message::ReadyForQuery(r) => {
                    self.tx = tx_status(r.status());
                    return Ok(());
                }
                _ => return Err("expected authentication request from server, but received other message\n".into()),
            }
            if !out.is_empty() {
                self.send(&out).map_err(|e| format!("{e}\n"))?;
            }
        }
    }

    fn send(&mut self, buf: &[u8]) -> std::io::Result<()> {
        let r = self.stream.write_all(buf).and_then(|_| self.stream.flush());
        if r.is_err() {
            self.bad = true;
        }
        r
    }

    /// Read more bytes into the buffer: blocking until `deadline` (None: for 100 ms at a
    /// time), or only what is there (`nonblocking`). Ok(false): nothing came.
    fn fill(&mut self, wait: Option<Duration>) -> std::io::Result<bool> {
        let mut tmp = [0u8; 16384];
        match wait {
            Some(d) => {
                self.stream.set_nonblocking(false)?;
                self.stream.set_read_timeout(Some(d.max(Duration::from_millis(1))))?;
            }
            None => self.stream.set_nonblocking(true)?,
        }
        let r = self.stream.read(&mut tmp);
        if wait.is_none() {
            let _ = self.stream.set_nonblocking(false);
        }
        match r {
            Ok(0) => {
                self.bad = true;
                Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "server closed the connection unexpectedly\n\tThis probably means the server terminated abnormally\n\tbefore or while processing the request.",
                ))
            }
            Ok(n) => {
                self.rbuf.extend_from_slice(&tmp[..n]);
                Ok(true)
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted) => Ok(false),
            Err(e) => {
                self.bad = true;
                Err(e)
            }
        }
    }

    /// The next message, waiting for it (until `deadline`).
    fn read_message(&mut self, deadline: Option<Instant>) -> std::io::Result<backend::Message> {
        loop {
            if let Some(m) = backend::Message::parse(&mut self.rbuf)? {
                return Ok(m);
            }
            let wait = match deadline {
                Some(d) => {
                    let left = d.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "timeout expired"));
                    }
                    left.min(Duration::from_millis(100))
                }
                None => Duration::from_millis(100),
            };
            self.fill(Some(wait))?;
        }
    }

    /// Take a message into the connection's state: results, notifications, parameters.
    fn process(&mut self, m: backend::Message) {
        match m {
            backend::Message::RowDescription(d) => {
                let mut r = PgResult::new(Status::TuplesOk);
                let mut it = d.fields();
                while let Ok(Some(f)) = fallible_iterator::FallibleIterator::next(&mut it) {
                    r.fields.push(Field {
                        name: f.name().to_string(),
                        type_oid: f.type_oid(),
                        typmod: f.type_modifier(),
                    });
                }
                self.cur = Some(r);
            }
            backend::Message::DataRow(d) => {
                let buf = d.buffer();
                let mut row = Vec::new();
                let mut it = d.ranges();
                while let Ok(Some(r)) = fallible_iterator::FallibleIterator::next(&mut it) {
                    row.push(r.map(|r| String::from_utf8_lossy(&buf[r]).into_owned()));
                }
                if let Some(c) = self.cur.as_mut() {
                    c.rows.push(row);
                }
            }
            backend::Message::CommandComplete(c) => {
                let mut r = self.cur.take().unwrap_or_else(|| PgResult::new(Status::CommandOk));
                r.cmd_status = c.tag().unwrap_or("").to_string();
                self.results.push_back(r);
            }
            backend::Message::EmptyQueryResponse => self.results.push_back(PgResult::new(Status::EmptyQuery)),
            backend::Message::PortalSuspended => {
                let r = self.cur.take().unwrap_or_else(|| PgResult::new(Status::TuplesOk));
                self.results.push_back(r);
            }
            backend::Message::ErrorResponse(e) => {
                self.cur = None;
                let f = error_fields(e.fields());
                self.error_message = f.text();
                let mut r = PgResult::new(Status::FatalError);
                r.error = Some(f);
                self.results.push_back(r);
            }
            backend::Message::CopyInResponse(_) => self.results.push_back(PgResult::new(Status::CopyIn)),
            backend::Message::CopyOutResponse(_) => self.results.push_back(PgResult::new(Status::CopyOut)),
            backend::Message::NotificationResponse(n) => self.notifies.push_back(Notify {
                channel: n.channel().unwrap_or("").to_string(),
                pid: n.process_id(),
                payload: n.message().unwrap_or("").to_string(),
            }),
            backend::Message::ParameterStatus(p) => {
                if let (Ok(n), Ok(v)) = (p.name(), p.value()) {
                    self.params.insert(n.to_string(), v.to_string());
                }
            }
            backend::Message::ReadyForQuery(r) => {
                self.tx = tx_status(r.status());
                self.ready = true;
            }
            // Notices of the remote session are logged, not passed on (libpq's default
            // notice processor writes them to the server's stderr).
            backend::Message::NoticeResponse(_) => {}
            _ => {}
        }
    }

    fn begin_send(&mut self) -> Result<(), String> {
        if self.bad {
            self.error_message = "connection to server was lost\n".into();
            return Err(self.error_message.clone());
        }
        if self.busy {
            self.error_message = "another command is already in progress\n".into();
            return Err(self.error_message.clone());
        }
        self.results.clear();
        self.cur = None;
        self.ready = false;
        self.error_message.clear();
        Ok(())
    }

    fn sent(&mut self, buf: &[u8]) -> Result<(), String> {
        if let Err(e) = self.send(buf) {
            self.error_message = format!("could not send data to server: {e}\n");
            return Err(self.error_message.clone());
        }
        self.busy = true;
        Ok(())
    }

    /// PQsendQuery: a simple-protocol query, results read with `get_result`.
    pub fn send_query(&mut self, sql: &str) -> Result<(), String> {
        self.begin_send()?;
        let mut buf = BytesMut::new();
        frontend::query(sql, &mut buf).map_err(|e| e.to_string())?;
        self.sent(&buf)
    }

    /// PQsendQueryParams / PQsendQueryPrepared (`name` Some): parameters and results in text.
    pub fn send_query_params(&mut self, sql: &str, name: Option<&str>, params: &[Option<String>]) -> Result<(), String> {
        self.begin_send()?;
        let mut buf = BytesMut::new();
        let io = |e: std::io::Error| e.to_string();
        if name.is_none() {
            frontend::parse("", sql, std::iter::empty(), &mut buf).map_err(io)?;
        }
        let r = frontend::bind(
            "",
            name.unwrap_or(""),
            std::iter::empty::<i16>(),
            params.iter(),
            |p, out| -> Result<postgres_protocol::IsNull, Box<dyn std::error::Error + Sync + Send>> {
                Ok(match p {
                    Some(s) => {
                        out.extend_from_slice(s.as_bytes());
                        postgres_protocol::IsNull::No
                    }
                    None => postgres_protocol::IsNull::Yes,
                })
            },
            std::iter::empty::<i16>(),
            &mut buf,
        );
        if r.is_err() {
            return Err("could not encode the query's parameters".into());
        }
        frontend::describe(b'P', "", &mut buf).map_err(io)?;
        frontend::execute("", 0, &mut buf).map_err(io)?;
        frontend::sync(&mut buf);
        self.sent(&buf)
    }

    /// PQprepare: parse `sql` as statement `name` with `nparams` parameters of unspecified
    /// types. The result is CommandOk or the error.
    pub fn prepare(&mut self, name: &str, sql: &str, intr: Interrupt) -> crate::Result<PgResult> {
        if let Err(e) = self.begin_send() {
            return Ok(self.failed(e));
        }
        let mut buf = BytesMut::new();
        let _ = frontend::parse(name, sql, std::iter::empty(), &mut buf);
        frontend::sync(&mut buf);
        if let Err(e) = self.sent(&buf) {
            return Ok(self.failed(e));
        }
        let mut last = PgResult::new(Status::CommandOk);
        while let Some(r) = self.get_result(intr)? {
            last = r;
        }
        Ok(last)
    }

    /// A result standing for a failure of the connection itself.
    fn failed(&mut self, msg: String) -> PgResult {
        self.error_message = msg;
        PgResult::new(Status::FatalError)
    }

    /// PQgetResult: the next result of the query sent, None after the last one.
    pub fn get_result(&mut self, intr: Interrupt) -> crate::Result<Option<PgResult>> {
        if !self.busy {
            return Ok(None);
        }
        loop {
            if let Some(r) = self.results.pop_front() {
                return Ok(Some(r));
            }
            if self.ready {
                self.busy = false;
                self.ready = false;
                return Ok(None);
            }
            match backend::Message::parse(&mut self.rbuf) {
                Ok(Some(m)) => {
                    self.process(m);
                    continue;
                }
                Ok(None) => {}
                Err(e) => return Ok(Some(self.lost(e))),
            }
            if let Err(e) = intr() {
                self.cancel_and_drain();
                return Err(e);
            }
            if let Err(e) = self.fill(Some(Duration::from_millis(100))) {
                return Ok(Some(self.lost(e)));
            }
        }
    }

    /// The connection broke while reading a query's results.
    fn lost(&mut self, e: std::io::Error) -> PgResult {
        self.bad = true;
        self.busy = false;
        self.ready = false;
        self.tx = TxStatus::Unknown;
        self.failed(format!("{e}\n"))
    }

    /// The query in progress is cancelled and its results read and dropped (what
    /// postgres_fdw's abort cleanup does), for up to 30 seconds.
    pub fn cancel_and_drain(&mut self) {
        if !self.busy {
            return;
        }
        let _ = self.cancel();
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.busy && !self.bad && Instant::now() < deadline {
            if let Ok(Some(m)) = backend::Message::parse(&mut self.rbuf) {
                self.process(m);
                self.results.clear();
                if self.ready {
                    self.busy = false;
                    self.ready = false;
                }
                continue;
            }
            if self.fill(Some(Duration::from_millis(100))).is_err() {
                break;
            }
        }
    }

    /// PQexec: send `sql`, read every result, return the last.
    pub fn exec(&mut self, sql: &str, intr: Interrupt) -> crate::Result<PgResult> {
        if let Err(e) = self.send_query(sql) {
            return Ok(self.failed(e));
        }
        self.last_result(intr)
    }

    /// PQexecParams / PQexecPrepared.
    pub fn exec_params(&mut self, sql: &str, name: Option<&str>, params: &[Option<String>], intr: Interrupt) -> crate::Result<PgResult> {
        if let Err(e) = self.send_query_params(sql, name, params) {
            return Ok(self.failed(e));
        }
        self.last_result(intr)
    }

    fn last_result(&mut self, intr: Interrupt) -> crate::Result<PgResult> {
        let mut last = None;
        while let Some(r) = self.get_result(intr)? {
            let stop = matches!(r.status, Status::CopyIn | Status::CopyOut) || self.bad;
            last = Some(r);
            if stop {
                break;
            }
        }
        Ok(last.unwrap_or_else(|| {
            let msg = if self.error_message.is_empty() { "no result\n".to_string() } else { self.error_message.clone() };
            self.failed(msg)
        }))
    }

    /// PQconsumeInput: read what has arrived without waiting. False: the connection broke.
    pub fn consume_input(&mut self) -> bool {
        if self.bad {
            return false;
        }
        loop {
            match self.fill(None) {
                Ok(true) => continue,
                Ok(false) => break,
                Err(e) => {
                    self.error_message = format!("{e}\n");
                    return false;
                }
            }
        }
        while let Ok(Some(m)) = backend::Message::parse(&mut self.rbuf) {
            self.process(m);
        }
        true
    }

    /// PQisBusy: whether get_result would wait.
    pub fn is_busy(&self) -> bool {
        self.busy && self.results.is_empty() && !self.ready
    }

    /// PQnotifies.
    pub fn notifies(&mut self) -> Option<Notify> {
        self.notifies.pop_front()
    }

    /// PQtransactionStatus.
    pub fn tx_status(&self) -> TxStatus {
        if self.bad {
            TxStatus::Unknown
        } else if self.busy {
            TxStatus::Active
        } else {
            self.tx
        }
    }

    /// PQcancel: a CancelRequest on a connection of its own.
    pub fn cancel(&self) -> Result<(), String> {
        let mut s = TcpStream::connect_timeout(&self.addr, Duration::from_secs(30)).map_err(|e| format!("{e}"))?;
        let mut buf = BytesMut::new();
        frontend::cancel_request(self.pid, self.secret, &mut buf);
        s.write_all(&buf).map_err(|e| format!("{e}"))?;
        let _ = s.set_read_timeout(Some(Duration::from_secs(30)));
        let mut tmp = [0u8; 16];
        let _ = s.read(&mut tmp);
        Ok(())
    }

    /// PQparameterStatus.
    pub fn parameter(&self, name: &str) -> Option<&str> {
        self.params.get(name).map(String::as_str)
    }

    // ── COPY and the replication protocol (libpqwalreceiver) ──

    /// The error text of the last failure, chomped (what libpqwalreceiver reports).
    pub fn error_text(&self) -> String {
        self.error_message.trim_end_matches('\n').to_string()
    }

    /// Send a command answered with CopyBothResponse (START_REPLICATION): Ok once the
    /// stream is open; Err the server's error (or the connection's failure).
    pub fn start_copy_both(&mut self, cmd: &str, intr: Interrupt) -> crate::Result<Result<(), String>> {
        if let Err(e) = self.send_query(cmd) {
            return Ok(Err(e.trim_end_matches('\n').to_string()));
        }
        loop {
            // (postgres-protocol doesn't parse CopyBothResponse: taken off here.)
            if self.rbuf.len() >= 5 && self.rbuf[0] == b'W' {
                let len = u32::from_be_bytes([self.rbuf[1], self.rbuf[2], self.rbuf[3], self.rbuf[4]]) as usize;
                if self.rbuf.len() > len {
                    let _ = self.rbuf.split_to(1 + len);
                    return Ok(Ok(()));
                }
            } else {
                match backend::Message::parse(&mut self.rbuf) {
                    Ok(Some(m)) => {
                        self.process(m);
                        if let Some(r) = self.results.pop_front() {
                            // An error (or an unexpected result): read up to ReadyForQuery.
                            while self.get_result(intr)?.is_some() {}
                            let msg = r.error.map_or_else(|| "unexpected result from server".to_string(), |e| e.text());
                            self.error_message = msg;
                            return Ok(Err(self.error_text()));
                        }
                        if self.ready {
                            self.busy = false;
                            self.ready = false;
                            return Ok(Err("unexpected end of the server's response".into()));
                        }
                        continue;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        self.lost(e);
                        return Ok(Err(self.error_text()));
                    }
                }
            }
            intr()?;
            if let Err(e) = self.fill(Some(Duration::from_millis(100))) {
                self.lost(e);
                return Ok(Err(self.error_text()));
            }
        }
    }

    /// The next message of a COPY OUT / COPY BOTH stream, waiting up to `wait`: Some(Some
    /// data), Some(None) at the server's CopyDone, None when nothing came. Err: the server's
    /// error or the connection's failure.
    pub fn copy_read(&mut self, wait: Duration) -> Result<Option<Option<bytes::Bytes>>, String> {
        let deadline = Instant::now() + wait;
        loop {
            match backend::Message::parse(&mut self.rbuf) {
                Ok(Some(backend::Message::CopyData(d))) => return Ok(Some(Some(d.into_bytes()))),
                Ok(Some(backend::Message::CopyDone)) => return Ok(Some(None)),
                Ok(Some(backend::Message::ErrorResponse(e))) => {
                    self.error_message = error_fields(e.fields()).text();
                    return Err(self.error_text());
                }
                Ok(Some(m)) => {
                    self.process(m);
                    continue;
                }
                Ok(None) => {}
                Err(e) => {
                    self.lost(e);
                    return Err(self.error_text());
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            if let Err(e) = self.fill(Some(left.min(Duration::from_millis(100)))) {
                self.lost(e);
                return Err(self.error_text());
            }
        }
    }

    /// PQputCopyData on a COPY BOTH stream.
    pub fn copy_send(&mut self, data: &[u8]) -> Result<(), String> {
        let mut buf = BytesMut::with_capacity(data.len() + 5);
        buf.extend_from_slice(b"d");
        buf.extend_from_slice(&((data.len() + 4) as u32).to_be_bytes());
        buf.extend_from_slice(data);
        self.send(&buf).map_err(|e| format!("could not send data to server: {e}"))
    }

    /// PQputCopyEnd: CopyDone.
    pub fn copy_done(&mut self) -> Result<(), String> {
        let mut buf = BytesMut::new();
        frontend::copy_done(&mut buf);
        self.send(&buf).map_err(|e| format!("could not send data to server: {e}"))
    }

    /// The results after a COPY OUT ended (its CommandComplete, up to ReadyForQuery): Err the
    /// server's error.
    pub fn copy_finish(&mut self, intr: Interrupt) -> crate::Result<Result<(), String>> {
        let mut err = None;
        while let Some(r) = self.get_result(intr)? {
            if r.status == Status::FatalError {
                err = Some(r.error.map_or_else(|| self.error_text(), |e| e.text().trim_end_matches('\n').to_string()));
            }
        }
        Ok(err.map_or(Ok(()), Err))
    }
}


impl Drop for Conn {
    /// PQfinish: a Terminate message, then the socket closes.
    fn drop(&mut self) {
        if !self.bad {
            let mut buf = BytesMut::new();
            frontend::terminate(&mut buf);
            let _ = self.stream.write_all(&buf);
        }
    }
}

fn tx_status(b: u8) -> TxStatus {
    match b {
        b'I' => TxStatus::Idle,
        b'T' => TxStatus::InTrans,
        b'E' => TxStatus::InError,
        _ => TxStatus::Unknown,
    }
}

fn error_fields(mut it: backend::ErrorFields<'_>) -> ErrorFields {
    let mut out = Vec::new();
    while let Ok(Some(x)) = fallible_iterator::FallibleIterator::next(&mut it) {
        out.push((x.type_(), String::from_utf8_lossy(x.value_bytes()).into_owned()));
    }
    ErrorFields(out)
}

//! The `postgresql` connector: every table of a schema with a primary key, read in key order in
//! one read-only snapshot per table, a bounded page at a time.
//!
//! The host is the tenant's choice: it is resolved here and refused when any of its addresses is
//! private (unless the node allows private hosts), and the connection goes to the address that
//! was checked. A row's `_id` is `{database}_{schema}_{table}_{key values joined by _}`; its
//! columns are fields `{schema}_{table}_{column}`, next to `database`, `schema`, `table` and
//! `_timestamp`. With an `incremental_column`, incremental syncs read only rows at or after the
//! cursor the last sync left (that column's maximum in its snapshot, per table).
//!
//! Memory is bounded per page whatever the table or the server holds:
//! - A page is a keyset query (`WHERE (key) > (last key) ORDER BY key LIMIT n`) whose rows the
//!   server cuts at [`PAGE_BYTES`] with a running sum of their sizes; a row over
//!   [`DOC_BYTES_MAX`] comes back as NULL (skipped and counted), so an honest page is at most
//!   `PAGE_BYTES + DOC_BYTES_MAX` of rows' text, and a page past that (or past its row count)
//!   fails the sync.
//! - A page's rows are streamed: each is turned into its document and dropped as it arrives
//!   (the client library hands rows over one message at a time and reads no further ahead), and
//!   the documents a page holds are charged to the cluster's request breaker until indexed.
//! - The connection counts the bytes it reads ([`Counted`]): a query whose answer passes its
//!   window ([`PAGE_WINDOW`]) fails the sync, so a server ignoring the cut can't grow a page.
//! - Every message's 5-byte header is read on the plaintext side ([`Framed`], under TLS through
//!   [`FramedTls`]) before the client library sees it: a length over [`PAGE_WINDOW`] fails the
//!   connection, since the library reserves a message's whole length from its header, and so
//!   does a row (or row description) of more than [`COLUMNS_MAX`] columns, since the library
//!   keeps a position per column of every row.
//! - The server's notices are dropped as they arrive (their bytes count in the window): none is
//!   kept, and none of a server's text reaches the node's logs.
//! - Every statement has a client-side timeout a little over the session's `statement_timeout`
//!   and races the job's cancel, and the socket has TCP keepalives: a silent server or a dead
//!   network ends the sync instead of holding its slot.

use super::config::Values;
use super::{DOC_BYTES_MAX, PAGE_BYTES, PAGE_DOCS, Sink, Stop};
use crate::json::{Map, Value};
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;
use futures_util::StreamExt;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_postgres::config::SslMode;
use tokio_postgres::tls::{MakeTlsConnect, TlsConnect, TlsStream};
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::CertificateDer;
use tokio_rustls::rustls::pki_types::pem::PemObject;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// What one statement may take on the server, and how long a transaction may sit idle (a sync
/// that stalls can't hold a snapshot open).
const SESSION: &str = "-c statement_timeout=300000 -c idle_in_transaction_session_timeout=600000 -c standard_conforming_strings=on";
/// The client's own limit on a statement: the server's plus some slack.
const QUERY_TIMEOUT: Duration = Duration::from_secs(330);
/// The most a page's answer may take on the wire (rows, their keys again, framing, TLS).
pub const PAGE_WINDOW: u64 = 2 * (PAGE_BYTES + DOC_BYTES_MAX) as u64 + (4 << 20);
/// The most a catalog query's answer may take (its rows are collected: a few hundred thousand
/// table names).
const CATALOG_WINDOW: u64 = 8 << 20;
/// The most columns a row may have: roll's queries answer a row's JSON and its primary key's
/// columns (an index has at most 32 in Postgres).
const COLUMNS_MAX: u16 = 64;
/// What a page's documents are charged to the breaker per byte of rows' text (parsed JSON).
const PARSED_FACTOR: u64 = 3;

/// `"name"`, quotes doubled.
fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// `'text'`, quotes doubled (`standard_conforming_strings` is on for the session).
fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// The host's first address, refusing a private one.
async fn resolve(host: &str, port: u16, allow_private: bool) -> Result<SocketAddr, String> {
    let addrs: Vec<SocketAddr> = match host.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::lookup_host((host, port)))
            .await
            .map_err(|_| format!("resolving [{host}] timed out"))?
            .map_err(|_| format!("[{host}] doesn't resolve"))?
            .collect(),
    };
    let addr = addrs.first().copied().ok_or_else(|| format!("[{host}] doesn't resolve"))?;
    if !allow_private && addrs.iter().any(|a| roll_net::private_ip(a.ip())) {
        return Err(format!("[{host}] is a private address: connectors only reach public ones"));
    }
    Ok(addr)
}

fn tls_config(ca: Option<&str>) -> Result<rustls::ClientConfig, String> {
    let mut roots = rustls::RootCertStore::empty();
    for c in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(c);
    }
    if let Some(pem) = ca {
        for c in CertificateDer::pem_slice_iter(pem.as_bytes()) {
            roots.add(c.map_err(|_| "[ssl_ca] isn't PEM certificates".to_string())?).map_err(|e| format!("[ssl_ca]: {e}"))?;
        }
    }
    Ok(rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("TLS: {e}"))?
        .with_root_certificates(roots)
        .with_no_client_auth())
}

/// The server's own message, or the client's error with its cause (an I/O error, a refused
/// frame); never the connection's settings.
fn db_error(e: tokio_postgres::Error) -> String {
    let text = if let Some(d) = e.as_db_error() {
        d.message().to_string()
    } else {
        match std::error::Error::source(&e) {
            Some(cause) => format!("{e}: {cause}"),
            None => e.to_string(),
        }
    };
    super::capped(text)
}

/// How many bytes the connection may still read before the current query fails.
#[derive(Clone, Default)]
struct Window {
    read: Arc<AtomicU64>,
    limit: Arc<AtomicU64>,
    /// Why the connection's task ended, when it failed (a refused frame, an I/O error): a query
    /// only sees the connection close.
    failure: Arc<parking_lot::Mutex<Option<String>>>,
    /// The server's notices, dropped as they arrived ([`drive`]).
    notices: Arc<AtomicU64>,
}

impl Window {
    /// Starts a new window of `bytes` (before each query).
    fn open(&self, bytes: u64) {
        self.read.store(0, Ordering::SeqCst);
        self.limit.store(bytes, Ordering::SeqCst);
    }
}

/// A TCP stream that fails a read past its [`Window`].
struct Counted {
    inner: tokio::net::TcpStream,
    window: Window,
}

impl AsyncRead for Counted {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let r = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = r {
            let n = (buf.filled().len() - before) as u64;
            let read = self.window.read.fetch_add(n, Ordering::SeqCst) + n;
            if read > self.window.limit.load(Ordering::SeqCst) {
                // A failed read hands back nothing (AsyncRead's contract).
                buf.set_filled(before);
                return Poll::Ready(Err(std::io::Error::other("the server sent more than one page may hold")));
            }
        }
        r
    }
}

impl AsyncWrite for Counted {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Runs a connection's I/O until it ends, recording why it failed. Its messages are taken one by
/// one rather than by awaiting the connection, which logs each notice the server sends: a
/// notice is counted and dropped.
async fn drive<S, T>(mut conn: tokio_postgres::Connection<S, T>, window: Window)
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        match std::future::poll_fn(|cx| conn.poll_message(cx)).await {
            Some(Ok(tokio_postgres::AsyncMessage::Notice(_))) => {
                window.notices.fetch_add(1, Ordering::SeqCst);
            }
            // Notifications: roll listens on no channel.
            Some(Ok(_)) => {}
            Some(Err(e)) => {
                *window.failure.lock() = Some(db_error(e));
                return;
            }
            None => return,
        }
    }
}

/// Postgres's framing of server messages: a type byte, then a 4-byte big-endian length that
/// counts itself.
#[derive(Default)]
struct Frames {
    header: [u8; 5],
    have: usize,
    /// Body bytes of the current message still to come.
    remaining: u64,
    /// `ParameterStatus` messages (`S`) so far, and their bytes: the client library keeps every
    /// one for the connection's life, so a server could grow it without end.
    params: u64,
    param_bytes: u64,
    /// The column count of the current row (`D`) or row description (`T`), as its first two body
    /// bytes arrive.
    columns: Option<(usize, [u8; 2])>,
}

/// The most `ParameterStatus` messages, and bytes of them, one connection takes (a server sends
/// about fifteen at startup and one per `SET` of a reported setting).
const PARAMS_MAX: u64 = 256;
const PARAM_BYTES_MAX: u64 = 64 << 10;

impl Frames {
    fn feed(&mut self, mut bytes: &[u8]) -> std::io::Result<()> {
        while !bytes.is_empty() {
            if self.remaining > 0 {
                if let Some((have, count)) = &mut self.columns {
                    count[*have] = bytes[0];
                    *have += 1;
                    self.remaining -= 1;
                    bytes = &bytes[1..];
                    if *have == 2 {
                        let n = u16::from_be_bytes(*count);
                        self.columns = None;
                        if n > COLUMNS_MAX {
                            return Err(std::io::Error::other(format!("the server sent a row of {n} columns: roll's queries answer at most {COLUMNS_MAX}")));
                        }
                    }
                    continue;
                }
                let n = (self.remaining.min(bytes.len() as u64)) as usize;
                self.remaining -= n as u64;
                bytes = &bytes[n..];
                continue;
            }
            let n = (5 - self.have).min(bytes.len());
            self.header[self.have..self.have + n].copy_from_slice(&bytes[..n]);
            self.have += n;
            bytes = &bytes[n..];
            if self.have == 5 {
                self.have = 0;
                let len = u32::from_be_bytes([self.header[1], self.header[2], self.header[3], self.header[4]]) as u64;
                if !(4..=PAGE_WINDOW).contains(&len) {
                    return Err(std::io::Error::other(format!("the server sent a message of {len} bytes: over what one page may hold")));
                }
                self.remaining = len - 4;
                if matches!(self.header[0], b'D' | b'T') && self.remaining >= 2 {
                    self.columns = Some((0, [0; 2]));
                }
                if self.header[0] == b'S' {
                    self.params += 1;
                    self.param_bytes += len;
                    if self.params > PARAMS_MAX || self.param_bytes > PARAM_BYTES_MAX {
                        return Err(std::io::Error::other("the server sent more parameter statuses than a connection keeps"));
                    }
                }
            }
        }
        Ok(())
    }
}

/// A plaintext stream of server messages whose headers are checked as they arrive ([`Frames`]).
struct Framed<S> {
    inner: S,
    frames: Frames,
}

impl<S> Framed<S> {
    fn new(inner: S) -> Framed<S> {
        Framed { inner, frames: Frames::default() }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Framed<S> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let r = Pin::new(&mut this.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = r
            && let Err(e) = this.frames.feed(&buf.filled()[before..])
        {
            buf.set_filled(before);
            return Poll::Ready(Err(e));
        }
        r
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Framed<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl<S: TlsStream + Unpin> TlsStream for Framed<S> {
    fn channel_binding(&self) -> tokio_postgres::tls::ChannelBinding {
        self.inner.channel_binding()
    }
}

/// A TLS connector whose sessions are [`Framed`] (the headers are only readable after TLS).
struct FramedTls<T>(T);

impl<S, T> TlsConnect<S> for FramedTls<T>
where
    T: TlsConnect<S>,
    T::Future: Send + 'static,
    T::Stream: Send + 'static,
    T::Error: Send + 'static,
{
    type Stream = Framed<T::Stream>;
    type Error = T::Error;
    type Future = Pin<Box<dyn std::future::Future<Output = Result<Framed<T::Stream>, T::Error>> + Send>>;

    fn connect(self, stream: S) -> Self::Future {
        let handshake = self.0.connect(stream);
        Box::pin(async move { handshake.await.map(Framed::new) })
    }
}

struct Conn {
    client: tokio_postgres::Client,
    window: Window,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn connect(v: &Values, allow_private: bool) -> Result<Conn, String> {
    let host = v.need("host")?;
    let port = u16::try_from(v.int("port", 5432)?).map_err(|_| "the connector's [port] is out of range".to_string())?;
    let mut cfg = tokio_postgres::Config::new();
    cfg.host(&host).port(port).user(v.need("username")?).dbname(v.need("database")?).application_name("roll search connector").options(SESSION);
    if let Some(p) = v.text("password") {
        cfg.password(p);
    }
    let addr = resolve(&host, port, allow_private).await?;
    let tcp = tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::TcpStream::connect(addr))
        .await
        .map_err(|_| format!("connecting to [{host}] timed out"))?
        .map_err(|e| format!("connecting to [{host}] failed: {e}"))?;
    let _ = tcp.set_nodelay(true);
    // A dead network or a vanished server is noticed within about a minute and a half.
    let keepalive = socket2::TcpKeepalive::new().with_time(Duration::from_secs(30)).with_interval(Duration::from_secs(10)).with_retries(6);
    let _ = socket2::SockRef::from(&tcp).set_tcp_keepalive(&keepalive);
    let window = Window::default();
    window.open(CATALOG_WINDOW);
    let stream = Counted { inner: tcp, window: window.clone() };
    let w = window.clone();
    let handshake = async {
        let connected = if v.bool("ssl_enabled", true) {
            cfg.ssl_mode(SslMode::Require);
            let mut make = tokio_postgres_rustls::MakeRustlsConnect::new(tls_config(v.text("ssl_ca").as_deref())?);
            let tls = MakeTlsConnect::<Counted>::make_tls_connect(&mut make, &host).map_err(|e| format!("TLS: {e}"))?;
            cfg.connect_raw(stream, FramedTls(tls)).await.map(|(c, conn)| (c, tokio::spawn(drive(conn, w))))
        } else {
            cfg.ssl_mode(SslMode::Disable);
            cfg.connect_raw(Framed::new(stream), tokio_postgres::NoTls).await.map(|(c, conn)| (c, tokio::spawn(drive(conn, w))))
        };
        connected.map_err(|e| format!("connecting to [{host}] failed: {}", db_error(e)))
    };
    let (client, task) = tokio::time::timeout(CONNECT_TIMEOUT, handshake).await.map_err(|_| format!("connecting to [{host}] timed out"))??;
    Ok(Conn { client, window, task })
}

/// `fut`, a statement, bounded by [`QUERY_TIMEOUT`] and stopped by the job's cancel. A statement
/// that failed because the connection closed says why the connection failed.
async fn wait<T>(sink: &Sink<'_>, window: &Window, what: &str, fut: impl std::future::Future<Output = Result<T, tokio_postgres::Error>>) -> Result<T, Stop> {
    tokio::select! {
        r = tokio::time::timeout(QUERY_TIMEOUT, fut) => match r {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => {
                let closed = e.is_closed();
                let mut message = db_error(e);
                if closed {
                    // The connection's task records its error as it ends, just after this.
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    if let Some(cause) = window.failure.lock().clone() {
                        message = format!("{message}: {cause}");
                    }
                }
                Err(Stop::Failed(format!("{what} failed: {message}")))
            }
            Err(_) => Err(Stop::Failed(format!("{what} timed out: the database didn't answer within {} s", QUERY_TIMEOUT.as_secs()))),
        },
        s = sink.stopped() => Err(s),
    }
}

/// A table to read: its name, primary key columns with their types, and the incremental
/// column's type (when it has it).
struct Table {
    name: String,
    key: Vec<(String, String)>,
    incremental: Option<String>,
}

async fn tables(conn: &Conn, sink: &Sink<'_>, schema: &str, wanted: &[String], incremental: Option<&str>) -> Result<(Vec<Table>, Vec<String>), Stop> {
    let c = &conn.client;
    conn.window.open(CATALOG_WINDOW);
    let rows = wait(
        sink,
        &conn.window,
        &format!("listing the tables of [{schema}]"),
        c.query("SELECT c.relname FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = $1 AND c.relkind IN ('r', 'p') ORDER BY 1", &[&schema]),
    )
    .await?;
    let all: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
    let names: Vec<String> = if wanted.is_empty() || wanted.iter().any(|t| t == "*") {
        all
    } else {
        if let Some(missing) = wanted.iter().find(|t| !all.contains(t)) {
            return Err(Stop::Failed(format!("the table [{schema}.{missing}] doesn't exist")));
        }
        wanted.to_vec()
    };
    let mut out = Vec::new();
    let mut skipped = Vec::new();
    for name in names {
        conn.window.open(CATALOG_WINDOW);
        let key: Vec<(String, String)> = wait(
            sink,
            &conn.window,
            &format!("reading the primary key of [{schema}.{name}]"),
            c.query(
                "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod) FROM pg_catalog.pg_index i JOIN pg_catalog.pg_class c ON c.oid = i.indrelid JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                 CROSS JOIN LATERAL unnest(i.indkey::int2[]) WITH ORDINALITY AS k(attnum, ord) JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid AND a.attnum = k.attnum \
                 WHERE i.indisprimary AND n.nspname = $1 AND c.relname = $2 ORDER BY k.ord",
                &[&schema, &name],
            ),
        )
        .await?
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
        if key.is_empty() {
            skipped.push(name);
            continue;
        }
        let incremental = match incremental {
            Some(col) => wait(
                sink,
                &conn.window,
                &format!("reading [{schema}.{name}]"),
                c.query_opt(
                    "SELECT format_type(a.atttypid, a.atttypmod) FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_class c ON c.oid = a.attrelid JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                     WHERE n.nspname = $1 AND c.relname = $2 AND a.attname = $3 AND a.attnum > 0 AND NOT a.attisdropped",
                    &[&schema, &name, &col],
                ),
            )
            .await?
            .map(|r| r.get::<_, String>(0)),
            None => None,
        };
        out.push(Table { name, key, incremental });
    }
    Ok((out, skipped))
}

/// Syncs the configured tables; the new cursor (`{"tables": {"schema.table": value}}`).
pub async fn sync(v: &Values, sink: &mut Sink<'_>, full: bool, cursor: &Value, allow_private: bool) -> Result<Value, Stop> {
    let database = v.need("database")?;
    let schema = v.text("schema").unwrap_or_else(|| "public".into());
    let incremental_col = v.text("incremental_column");
    let fetch = (v.int("fetch_size", 50)?.clamp(1, PAGE_DOCS as i64)) as usize;
    let mut conn = tokio::select! {
        c = connect(v, allow_private) => c?,
        s = sink.stopped() => return Err(s),
    };
    let (tables, skipped) = tables(&conn, sink, &schema, &v.list("tables"), incremental_col.as_deref()).await?;
    if !skipped.is_empty() {
        sink.counters.metadata.insert("skipped_tables".into(), Value::Arr(skipped.into_iter().map(Value::from).collect()));
    }
    let mut next = cursor.get("tables").and_then(Value::as_object).cloned().unwrap_or_default();
    let ts = super::now_nanos_string();
    for t in &tables {
        let key = format!("{schema}.{}", t.name);
        let since = if full { None } else { next.get(&key).and_then(Value::as_str).map(str::to_string) };
        let read = Read { database: &database, schema: &schema, table: t, col: incremental_col.as_deref(), since: since.as_deref(), fetch, ts: &ts };
        if let Some(max) = read_table(&mut conn, sink, &read).await? {
            next.insert(key, max.into());
        }
    }
    Ok(crate::obj! { "tables" => Value::Obj(next) })
}

/// What [`read_table`] reads.
struct Read<'a> {
    database: &'a str,
    schema: &'a str,
    table: &'a Table,
    col: Option<&'a str>,
    since: Option<&'a str>,
    fetch: usize,
    ts: &'a str,
}

/// A page's query: at most `n` rows after `after` (the last key's values as text), cut by the
/// server once their sizes pass [`PAGE_BYTES`] (always at least one row). It answers each row's
/// JSON (NULL when over [`DOC_BYTES_MAX`]) and its key values as text.
fn page_sql(r: &Read, qualified: &str, after: Option<&[String]>, n: usize) -> String {
    let t = r.table;
    let mut conds = Vec::new();
    if let Some(vals) = after {
        let cols = t.key.iter().map(|(k, _)| format!("t.{}", ident(k))).collect::<Vec<_>>().join(", ");
        let lits = t.key.iter().zip(vals).map(|((_, ty), v)| format!("{}::{ty}", literal(v))).collect::<Vec<_>>().join(", ");
        conds.push(format!("({cols}) > ({lits})"));
    }
    if let (Some(c), Some(ty), Some(s)) = (r.col, &t.incremental, r.since) {
        conds.push(format!("t.{} >= {}::{ty}", ident(c), literal(s)));
    }
    let filter = if conds.is_empty() { String::new() } else { format!(" WHERE {}", conds.join(" AND ")) };
    let order = t.key.iter().map(|(k, _)| format!("t.{}", ident(k))).collect::<Vec<_>>().join(", ");
    let ocols = (0..t.key.len()).map(|i| format!("o{i}")).collect::<Vec<_>>().join(", ");
    let kcols = (0..t.key.len()).map(|i| format!("k{i}")).collect::<Vec<_>>().join(", ");
    let inner_keys = t.key.iter().enumerate().map(|(i, (k, _))| format!("t.{c} AS o{i}, t.{c}::text AS k{i}", c = ident(k))).collect::<Vec<_>>().join(", ");
    let over = DOC_BYTES_MAX + 1;
    format!(
        "SELECT CASE WHEN len > {DOC_BYTES_MAX} THEN NULL ELSE j END, {kcols} FROM (\
           SELECT j, len, {kcols}, {ocols}, sum(least(len, {over})) OVER (ORDER BY {ocols} ROWS UNBOUNDED PRECEDING) - least(len, {over}) AS before FROM (\
             SELECT j, octet_length(j) AS len, {kcols}, {ocols} FROM (\
               SELECT row_to_json(t)::text AS j, {inner_keys} FROM {qualified} t{filter} ORDER BY {order} LIMIT {n}\
             ) a\
           ) b\
         ) c WHERE before < {PAGE_BYTES} ORDER BY {ocols}"
    )
}

/// Reads one table in one snapshot: the incremental column's maximum there, when it has one.
async fn read_table(conn: &mut Conn, sink: &mut Sink<'_>, r: &Read<'_>) -> Result<Option<String>, Stop> {
    let t = r.table;
    let label = format!("reading [{}.{}]", r.schema, t.name);
    let qualified = format!("{}.{}", ident(r.schema), ident(&t.name));
    let window = conn.window.clone();
    window.open(CATALOG_WINDOW);
    let tx = wait(sink, &window, &label, conn.client.build_transaction().isolation_level(tokio_postgres::IsolationLevel::RepeatableRead).read_only(true).start()).await?;
    let max: Option<String> = match (r.col, &t.incremental) {
        (Some(c), Some(_)) => wait(sink, &window, &label, tx.query_one(&format!("SELECT max({})::text FROM {qualified}", ident(c)), &[])).await?.get(0),
        _ => None,
    };
    let prefix = format!("{}_{}_", r.schema, t.name);
    let mut after: Option<Vec<String>> = None;
    // Pages ask for as many rows as the largest row seen leaves room for.
    let mut largest = DOC_BYTES_MAX;
    loop {
        let n = (PAGE_BYTES / largest.max(1024)).clamp(1, r.fetch);
        window.open(PAGE_WINDOW);
        let failed = |e: tokio_postgres::Error| Stop::Failed(format!("{label} failed: {}", db_error(e)));
        let rows = wait(sink, &window, &label, tx.query_raw(&page_sql(r, &qualified, after.as_deref(), n), std::iter::empty::<&(dyn tokio_postgres::types::ToSql + Sync)>())).await?;
        let mut rows = std::pin::pin!(rows);
        let mut docs = Vec::new();
        let mut held = sink.reserve();
        let (mut got, mut text) = (0usize, 0usize);
        largest = 0;
        while let Some(row) = wait(sink, &window, &label, async { rows.as_mut().next().await.transpose() }).await? {
            got += 1;
            let keys = (0..t.key.len()).map(|i| row.try_get::<_, String>(i + 1)).collect::<Result<Vec<_>, _>>().map_err(failed)?;
            let j = row.try_get::<_, Option<String>>(0).map_err(failed)?;
            drop(row);
            after = Some(keys);
            let Some(j) = j else {
                sink.skip();
                largest = DOC_BYTES_MAX;
                continue;
            };
            text += j.len();
            // The server cuts a page at PAGE_BYTES of rows (the last may add a row's limit).
            if got > n || text > PAGE_BYTES + DOC_BYTES_MAX {
                return Err(Stop::Failed(format!("{label} failed: the server sent more than one page may hold")));
            }
            held.add(j.len() as u64 * PARSED_FACTOR, "connector page").map_err(|e| Stop::Failed(format!("{label} failed: {}", e.reason())))?;
            largest = largest.max(j.len());
            let Ok(Some(Value::Obj(cols))) = crate::json::parse(j.as_bytes()) else {
                sink.skip();
                continue;
            };
            drop(j);
            let ids: Vec<String> = t.key.iter().map(|(k, _)| cols.get(k).map(Value::to_es_string).unwrap_or_default()).collect();
            let mut doc = Map::new();
            doc.insert("_id".into(), format!("{}_{}_{}_{}", r.database, r.schema, t.name, ids.join("_")).into());
            doc.insert("_timestamp".into(), r.ts.into());
            doc.insert("database".into(), r.database.into());
            doc.insert("schema".into(), r.schema.into());
            doc.insert("table".into(), t.name.clone().into());
            for (k, v) in cols {
                doc.insert(format!("{prefix}{k}"), v);
            }
            docs.push(doc);
        }
        if got == 0 {
            break;
        }
        sink.page(docs).await?;
        drop(held);
    }
    window.open(CATALOG_WINDOW);
    wait(sink, &window, &label, tx.commit()).await?;
    Ok(max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A server announcing parameter after parameter fails the connection once past the cap
    /// (the client library keeps every one).
    #[test]
    fn parameter_status_messages_are_capped() {
        let mut frames = Frames::default();
        let one = |name: &str| {
            let body = format!("{name}\0on\0");
            let mut m = vec![b'S'];
            m.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
            m.extend_from_slice(body.as_bytes());
            m
        };
        for i in 0..PARAMS_MAX {
            frames.feed(&one(&format!("p{i}"))).unwrap();
        }
        let err = frames.feed(&one("one_too_many")).unwrap_err();
        assert!(err.to_string().contains("parameter statuses"), "{err}");
    }

    /// A row (or row description) of more columns than roll's queries answer fails the
    /// connection before the client library keeps a position for each.
    #[test]
    fn rows_of_too_many_columns_are_refused() {
        let message = |kind: u8, columns: u16| {
            let mut m = vec![kind];
            m.extend_from_slice(&(4u32 + 2 + 4 * columns as u32).to_be_bytes());
            m.extend_from_slice(&columns.to_be_bytes());
            for _ in 0..columns {
                m.extend_from_slice(&(-1i32).to_be_bytes());
            }
            m
        };
        let mut frames = Frames::default();
        frames.feed(&message(b'D', 3)).unwrap();
        // Split anywhere, the count is still read.
        let big = message(b'T', 2000);
        frames.feed(&big[..6]).unwrap();
        let err = frames.feed(&big[6..]).unwrap_err();
        assert!(err.to_string().contains("2000 columns"), "{err}");
    }

    /// The server's notices are taken and dropped by the connection's task (never logged, never
    /// kept), and the connection goes on.
    #[tokio::test]
    async fn notices_are_dropped_not_logged() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let len = s.read_i32().await.unwrap();
            let mut startup = vec![0; len as usize - 4];
            s.read_exact(&mut startup).await.unwrap();
            // AuthenticationOk, ReadyForQuery.
            s.write_all(&[b'R', 0, 0, 0, 8, 0, 0, 0, 0, b'Z', 0, 0, 0, 5, b'I']).await.unwrap();
            let body = b"SNOTICE\0C00000\0Mthe server's own text\0\0";
            for _ in 0..3 {
                let mut m = vec![b'N'];
                m.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
                m.extend_from_slice(body);
                s.write_all(&m).await.unwrap();
            }
            s.shutdown().await.unwrap();
            let mut rest = Vec::new();
            let _ = s.read_to_end(&mut rest).await;
        });
        let mut cfg = tokio_postgres::Config::new();
        cfg.host("127.0.0.1").user("roll").dbname("shop").ssl_mode(SslMode::Disable);
        let window = Window::default();
        window.open(1 << 20);
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let stream = Framed::new(Counted { inner: tcp, window: window.clone() });
        let (_client, conn) = cfg.connect_raw(stream, tokio_postgres::NoTls).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), drive(conn, window.clone())).await.expect("the connection ends");
        assert_eq!(window.notices.load(Ordering::SeqCst), 3);
    }

    /// A TLS session in name only: the plaintext stream after a handshake (what [`FramedTls`]
    /// wraps under rustls).
    struct PlainTls;
    struct PlainStream(tokio::net::TcpStream);

    impl AsyncRead for PlainStream {
        fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.0).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for PlainStream {
        fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.0).poll_write(cx, buf)
        }
        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.0).poll_flush(cx)
        }
        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.0).poll_shutdown(cx)
        }
    }

    impl TlsStream for PlainStream {
        fn channel_binding(&self) -> tokio_postgres::tls::ChannelBinding {
            tokio_postgres::tls::ChannelBinding::none()
        }
    }

    impl TlsConnect<tokio::net::TcpStream> for PlainTls {
        type Stream = PlainStream;
        type Error = std::io::Error;
        type Future = std::future::Ready<Result<PlainStream, std::io::Error>>;
        fn connect(self, stream: tokio::net::TcpStream) -> Self::Future {
            std::future::ready(Ok(PlainStream(stream)))
        }
    }

    /// Under TLS the headers are checked on the session's plaintext: a huge one fails the
    /// connection before the client library reserves it.
    #[tokio::test]
    async fn framed_tls_refuses_a_huge_header() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut ssl_request = [0u8; 8];
            s.read_exact(&mut ssl_request).await.unwrap();
            s.write_all(b"S").await.unwrap();
            let len = s.read_i32().await.unwrap();
            let mut startup = vec![0; len as usize - 4];
            s.read_exact(&mut startup).await.unwrap();
            s.write_all(&[b'D', 0xFF, 0xFF, 0xFF, 0xF0]).await.unwrap();
            let mut rest = Vec::new();
            let _ = s.read_to_end(&mut rest).await;
        });
        let mut cfg = tokio_postgres::Config::new();
        cfg.host("127.0.0.1").user("roll").dbname("shop").ssl_mode(SslMode::Require);
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let err = tokio::time::timeout(Duration::from_secs(5), cfg.connect_raw(tcp, FramedTls(PlainTls))).await.expect("refused at once").err().expect("refused");
        assert!(db_error(err).contains("4294967280 bytes"));
    }

    /// A server sending more than a query's window fails the read instead of growing a buffer.
    #[tokio::test]
    async fn counted_stream_fails_past_its_window() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let _ = s.write_all(&vec![7u8; 3 << 20]).await;
        });
        let window = Window::default();
        window.open(1 << 20);
        let mut stream = Counted { inner: tokio::net::TcpStream::connect(addr).await.unwrap(), window: window.clone() };
        let mut got = Vec::new();
        let err = stream.read_to_end(&mut got).await.unwrap_err();
        assert!(err.to_string().contains("more than one page"), "{err}");
        // A new window starts from zero.
        window.open(1 << 20);
        assert_eq!(window.read.load(Ordering::SeqCst), 0);
    }
}

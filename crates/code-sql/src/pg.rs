//! The Postgres backend: a pool of connections driven by a runtime of this crate's own.
//!
//! Callers block on their requests with `futures::executor::block_on`: a client request only
//! talks to its connection's task through channels, and that task runs on our runtime, so this
//! works from any thread, inside another runtime's tasks included, and can't deadlock it.

use crate::{Error, Isolation, PgOptions, Result, ValueRef, translate};
use bytes::{BufMut, BytesMut};
use parking_lot::{Condvar, Mutex};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio_postgres::config::SslMode;
use tokio_postgres::types::{FromSql, IsNull, ToSql, Type, to_sql_checked};
use tokio_postgres::{Client, NoTls, Statement};
use tokio_postgres_rustls::MakeRustlsConnect;
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::CertificateDer;
use tokio_rustls::rustls::pki_types::pem::PemObject;

/// Prepared statements kept per connection.
const STATEMENTS: usize = 512;
/// Times a write is re-run after serialization failures and deadlocks.
const MAX_CONFLICT_RETRIES: u32 = 40;
/// Times a read or a write is re-run after its connection dropped (before any commit).
const MAX_CONNECTION_RETRIES: u32 = 5;
/// Rows pipelined at once by `execute_many`.
const PIPELINE: usize = 1024;

fn rt() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("code-sql-pg")
            .enable_all()
            .build()
            .expect("the Postgres runtime starts")
    })
}

/// Run `f` on our runtime and wait for it from any thread.
fn run<F>(f: F) -> F::Output
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    match futures::executor::block_on(rt().spawn(f)) {
        Ok(v) => v,
        Err(e) => std::panic::resume_unwind(e.into_panic()),
    }
}

/// Wait for a client request on this thread.
fn wait<F: Future>(f: F) -> F::Output {
    futures::executor::block_on(f)
}

pub(crate) fn describe(e: &tokio_postgres::Error) -> String {
    match e.as_db_error() {
        Some(db) => {
            let mut s = format!("postgres {}: {}", db.code().code(), db.message());
            if let Some(d) = db.detail() {
                s.push_str(&format!(" ({d})"));
            }
            s
        }
        None => {
            let mut s = format!("postgres: {e}");
            let mut src = std::error::Error::source(e);
            while let Some(c) = src {
                s.push_str(&format!(": {c}"));
                src = c.source();
            }
            s
        }
    }
}

fn redact(url: &str) -> String {
    match (url.find("://"), url.rfind('@')) {
        (Some(s), Some(at)) if at > s => format!("{}://…@{}", &url[..s], &url[at + 1..]),
        _ => url.to_string(),
    }
}

fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn tls_config(ca: Option<&std::path::Path>) -> Result<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    for c in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(c);
    }
    if let Some(ca) = ca {
        let certs = CertificateDer::pem_file_iter(ca).map_err(|e| Error::Other(format!("{}: reading CA certificates: {e}", ca.display())))?;
        for c in certs {
            let c = c.map_err(|e| Error::Other(format!("{}: bad CA certificate: {e}", ca.display())))?;
            roots.add(c).map_err(|e| Error::Other(format!("{}: bad CA certificate: {e}", ca.display())))?;
        }
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    Ok(rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Other(format!("TLS: {e}")))?
        .with_root_certificates(roots)
        .with_no_client_auth())
}

pub(crate) struct Pool {
    inner: Arc<PoolInner>,
}

struct PoolInner {
    config: tokio_postgres::Config,
    tls: Option<MakeRustlsConnect>,
    schema: String,
    opts: PgOptions,
    url: String,
    state: Mutex<PoolState>,
    freed: Condvar,
    listeners: Mutex<HashMap<String, tokio::sync::watch::Sender<u64>>>,
}

struct PoolState {
    idle: Vec<PgConn>,
    open: usize,
}

impl Pool {
    pub(crate) fn connect(url: &str, schema: &str, opts: PgOptions) -> Result<Pool> {
        let mut config: tokio_postgres::Config = url.parse().map_err(|e| Error::Other(format!("bad Postgres URL {}: {e}", redact(url))))?;
        config
            .connect_timeout(Duration::from_secs(10))
            .keepalives(true)
            .keepalives_idle(Duration::from_secs(15))
            .keepalives_interval(Duration::from_secs(5))
            .keepalives_retries(3)
            .tcp_user_timeout(Duration::from_secs(30));
        let tls = match config.get_ssl_mode() {
            SslMode::Disable => None,
            _ => Some(MakeRustlsConnect::new(tls_config(opts.ca.as_deref())?)),
        };
        if schema.is_empty() || schema.len() > 63 || !schema.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return Err(Error::Other(format!("bad schema name {schema:?}: use 1-63 letters, digits and _")));
        }
        // Which schema a connection serves shows in pg_stat_activity.
        config.application_name(format!("roll-code {schema}"));
        let pool = Pool {
            inner: Arc::new(PoolInner {
                config,
                tls,
                schema: schema.to_string(),
                opts,
                url: redact(url),
                state: Mutex::new(PoolState { idle: Vec::new(), open: 0 }),
                freed: Condvar::new(),
                listeners: Mutex::new(HashMap::new()),
            }),
        };
        // Connect now, so a bad URL fails at startup, and make sure the schema exists. Nodes
        // starting together may race to create it.
        let first = pool.inner.open_conn(false)?;
        let create = format!("CREATE SCHEMA IF NOT EXISTS {}", quote_ident(schema));
        if let Err(e) = wait(first.client.batch_execute(&create))
            && !matches!(e.code().map(|c| c.code()), Some("23505" | "42P06"))
        {
            return Err(e.into());
        }
        drop(first);
        Ok(pool)
    }

    pub(crate) fn schema(&self) -> &str {
        &self.inner.schema
    }

    fn get(&self) -> Result<Lease<'_>> {
        let inner = &*self.inner;
        let deadline = Instant::now() + inner.opts.acquire_timeout;
        let mut st = inner.state.lock();
        loop {
            while let Some(c) = st.idle.pop() {
                if !c.client.is_closed() {
                    return Ok(Lease { pool: inner, conn: Some(c) });
                }
                st.open -= 1;
            }
            if st.open < inner.opts.max_connections.max(1) {
                st.open += 1;
                drop(st);
                return match inner.open_conn(true) {
                    Ok(c) => Ok(Lease { pool: inner, conn: Some(c) }),
                    Err(e) => {
                        inner.state.lock().open -= 1;
                        inner.freed.notify_one();
                        Err(e)
                    }
                };
            }
            if inner.freed.wait_until(&mut st, deadline).timed_out() {
                return Err(Error::PoolTimeout(inner.opts.acquire_timeout));
            }
        }
    }

    pub(crate) fn read<T, E: From<Error>>(&self, mut f: impl FnMut(&PgConn) -> Result<T, E>) -> Result<T, E> {
        let mut attempt = 0;
        loop {
            let lease = self.get()?;
            let c = lease.conn();
            c.reset_flags();
            match f(c) {
                Ok(v) => return Ok(v),
                Err(e) => {
                    if c.broken.get() && attempt < MAX_CONNECTION_RETRIES {
                        attempt += 1;
                        tracing::warn!(db = %self.inner.url, attempt, "database connection lost during a read; retrying");
                        drop(lease);
                        backoff(attempt);
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }

    pub(crate) fn write<T, E: From<Error>>(&self, iso: Isolation, mut f: impl FnMut(&PgConn) -> Result<T, E>) -> Result<T, E> {
        let begin = match iso {
            Isolation::Serializable => "BEGIN ISOLATION LEVEL SERIALIZABLE",
            Isolation::ReadCommitted => "BEGIN ISOLATION LEVEL READ COMMITTED",
        };
        let (mut conflicts, mut drops) = (0, 0);
        loop {
            let lease = self.get()?;
            let c = lease.conn();
            c.reset_flags();
            let again = |c: &PgConn, conflicts: &mut u32, drops: &mut u32| -> bool {
                if c.broken.get() {
                    *drops += 1;
                    *drops <= MAX_CONNECTION_RETRIES
                } else if c.conflict.get() {
                    *conflicts += 1;
                    *conflicts <= MAX_CONFLICT_RETRIES
                } else {
                    false
                }
            };
            if let Err(e) = wait(c.client.batch_execute(begin)) {
                let e = c.note(e);
                if again(c, &mut conflicts, &mut drops) {
                    drop(lease);
                    backoff(drops.max(conflicts));
                    continue;
                }
                return Err(e.into());
            }
            c.in_tx.set(true);
            let out = f(c);
            let out = match out {
                Ok(v) if c.failed.borrow().is_some() => {
                    // A statement failed and `f` carried on: Postgres has aborted the transaction,
                    // and COMMIT would silently roll it back. Never report that as success.
                    let first = c.failed.borrow_mut().take().unwrap_or_default();
                    drop(v);
                    Err(Error::Other(format!("a statement failed inside the transaction, so nothing was written: {first}")))
                }
                Ok(v) => match wait(c.client.batch_execute("COMMIT")) {
                    Ok(()) => {
                        c.in_tx.set(false);
                        return Ok(v);
                    }
                    Err(e) => {
                        let e = c.note(e);
                        if c.broken.get() {
                            // The COMMIT may have reached the server.
                            return Err(Error::CommitUnknown(e.to_string()).into());
                        }
                        // The server answered: the transaction was rolled back.
                        c.in_tx.set(false);
                        if again(c, &mut conflicts, &mut drops) {
                            drop(lease);
                            backoff(conflicts);
                            continue;
                        }
                        return Err(e.into());
                    }
                },
                Err(e) => {
                    if !c.broken.get() {
                        match wait(c.client.batch_execute("ROLLBACK")) {
                            Ok(()) => c.in_tx.set(false),
                            Err(e) => drop(c.note(e)),
                        }
                    }
                    if again(c, &mut conflicts, &mut drops) {
                        drop(lease);
                        backoff(drops.max(conflicts));
                        continue;
                    }
                    return Err(e);
                }
            };
            // Only the failed-statement case gets here.
            if !c.broken.get() {
                match wait(c.client.batch_execute("ROLLBACK")) {
                    Ok(()) => c.in_tx.set(false),
                    Err(e) => drop(c.note(e)),
                }
            }
            if again(c, &mut conflicts, &mut drops) {
                drop(lease);
                backoff(drops.max(conflicts));
                continue;
            }
            return out.map_err(E::from);
        }
    }

    /// A read-committed write under the schema's advisory lock: schema changes and migrations,
    /// one node at a time.
    pub(crate) fn locked<T>(&self, mut f: impl FnMut(&PgConn) -> Result<T>) -> Result<T> {
        let key = schema_lock_key(&self.inner.schema);
        self.write(Isolation::ReadCommitted, |c| {
            c.execute("SELECT pg_advisory_xact_lock(?)", &[ValueRef::Int(key)])?;
            f(c)
        })
    }

    pub(crate) fn ensure_schema(&self, statements: &[String]) -> Result<()> {
        self.locked(|c| {
            for s in statements {
                c.batch(s)?;
            }
            Ok(())
        })
    }

    pub(crate) fn listen(&self, channel: &str) -> tokio::sync::watch::Receiver<u64> {
        let mut listeners = self.inner.listeners.lock();
        if let Some(tx) = listeners.get(channel)
            && !tx.is_closed()
        {
            return tx.subscribe();
        }
        let (tx, rx) = tokio::sync::watch::channel(0u64);
        listeners.insert(channel.to_string(), tx.clone());
        let (config, tls, url, channel) = (self.inner.config.clone(), self.inner.tls.clone(), self.inner.url.clone(), channel.to_string());
        rt().spawn(listen_loop(config, tls, url, channel, tx));
        rx
    }
}

/// Keep a connection listening on `channel`, bumping `tx` on every notification and after every
/// reconnect, until nobody holds a receiver.
async fn listen_loop(config: tokio_postgres::Config, tls: Option<MakeRustlsConnect>, url: String, channel: String, tx: tokio::sync::watch::Sender<u64>) {
    use futures::StreamExt;
    let bump = |tx: &tokio::sync::watch::Sender<u64>| tx.send_modify(|n| *n = n.wrapping_add(1));
    loop {
        if tx.is_closed() {
            return;
        }
        let listen = format!("LISTEN {}", quote_ident(&channel));
        let session = async {
            let (client, mut messages) = match &tls {
                Some(t) => {
                    let (client, mut conn) = config.connect(t.clone()).await?;
                    (client, futures::stream::poll_fn(move |cx| conn.poll_message(cx)).boxed())
                }
                None => {
                    let (client, mut conn) = config.connect(NoTls).await?;
                    (client, futures::stream::poll_fn(move |cx| conn.poll_message(cx)).boxed())
                }
            };
            // The connection only makes progress while its messages are polled.
            let (done_tx, mut done_rx) = tokio::sync::oneshot::channel::<()>();
            let tx2 = tx.clone();
            let pump = tokio::spawn(async move {
                while let Some(m) = messages.next().await {
                    match m {
                        Ok(tokio_postgres::AsyncMessage::Notification(_)) => bump(&tx2),
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                let _ = done_tx.send(());
            });
            client.batch_execute(&listen).await?;
            // Anything sent while we weren't listening was missed: wake everyone to look.
            bump(&tx);
            loop {
                tokio::select! {
                    _ = &mut done_rx => break,
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {
                        if tx.is_closed() {
                            pump.abort();
                            return Ok::<bool, tokio_postgres::Error>(true);
                        }
                    }
                }
            }
            drop(client);
            Ok(false)
        };
        match session.await {
            Ok(true) => return,
            Ok(false) => tracing::warn!(db = %url, channel = %channel, "lost the LISTEN connection; reconnecting"),
            Err(e) => tracing::warn!(db = %url, channel = %channel, error = %describe(&e), "LISTEN failed; retrying"),
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn schema_lock_key(schema: &str) -> i64 {
    // FNV-1a: stable across builds and nodes.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in b"roll-code-schema:".iter().chain(schema.as_bytes()) {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h as i64
}

fn backoff(attempt: u32) {
    // Up to 100 ms, half of it random, so conflicting writers don't retry in step.
    let ms = (1u64 << attempt.min(7)).min(100);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos() as u64).unwrap_or(0);
    std::thread::sleep(Duration::from_micros(ms * 500 + nanos % (ms * 500)));
}

impl PoolInner {
    fn open_conn(&self, pooled: bool) -> Result<PgConn> {
        let (config, tls) = (self.config.clone(), self.tls.clone());
        let timeout = self.opts.statement_timeout.as_millis();
        let setup = format!(
            "SET search_path TO {}; SET statement_timeout = {timeout}; SET idle_in_transaction_session_timeout = {}; SET lock_timeout = {timeout}",
            quote_ident(&self.schema),
            timeout * 2
        );
        let setup = if pooled { setup } else { format!("SET statement_timeout = {timeout}") };
        let url = self.url.clone();
        let client = run(async move {
            let client = match tls {
                Some(t) => {
                    let (client, conn) = config.connect(t).await?;
                    tokio::spawn(watch(conn, url));
                    client
                }
                None => {
                    let (client, conn) = config.connect(NoTls).await?;
                    tokio::spawn(watch(conn, url));
                    client
                }
            };
            client.batch_execute(&setup).await?;
            Ok::<_, tokio_postgres::Error>(client)
        })?;
        Ok(PgConn::new(client))
    }
}

async fn watch<F: Future<Output = Result<(), tokio_postgres::Error>>>(conn: F, url: String) {
    if let Err(e) = conn.await {
        tracing::warn!(db = %url, error = %describe(&e), "database connection lost");
    }
}

struct Lease<'a> {
    pool: &'a PoolInner,
    conn: Option<PgConn>,
}

impl Lease<'_> {
    fn conn(&self) -> &PgConn {
        self.conn.as_ref().expect("leased")
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        let Some(c) = self.conn.take() else { return };
        let mut st = self.pool.state.lock();
        // A connection still in a transaction (a panic in the closure) is dropped: the server
        // rolls it back.
        if c.broken.get() || c.in_tx.get() || c.client.is_closed() {
            st.open -= 1;
        } else {
            st.idle.push(c);
        }
        drop(st);
        self.pool.freed.notify_one();
    }
}

pub(crate) struct PgConn {
    client: Client,
    statements: RefCell<HashMap<String, Statement>>,
    /// The connection is unusable (closed, or its state unknown).
    broken: Cell<bool>,
    /// A serialization failure or deadlock: the transaction can be re-run.
    conflict: Cell<bool>,
    in_tx: Cell<bool>,
    /// The first statement that failed in the current transaction.
    failed: RefCell<Option<String>>,
}

impl PgConn {
    fn new(client: Client) -> PgConn {
        PgConn {
            client,
            statements: RefCell::new(HashMap::new()),
            broken: Cell::new(false),
            conflict: Cell::new(false),
            in_tx: Cell::new(false),
            failed: RefCell::new(None),
        }
    }

    fn reset_flags(&self) {
        self.conflict.set(false);
        *self.failed.borrow_mut() = None;
    }

    /// Classify an error: remember whether the transaction can be re-run.
    fn note(&self, e: tokio_postgres::Error) -> Error {
        match e.code().map(|c| c.code()) {
            Some("40001" | "40P01") => self.conflict.set(true),
            // A cached plan went stale after a schema change (another node migrating), or a
            // statement vanished: forget them all and re-run the transaction.
            Some("0A000" | "26000") => {
                self.statements.borrow_mut().clear();
                self.conflict.set(true);
            }
            _ => {}
        }
        if e.is_closed() || self.client.is_closed() {
            self.broken.set(true);
        }
        let err = Error::Postgres(e);
        if self.in_tx.get() {
            let mut failed = self.failed.borrow_mut();
            if failed.is_none() {
                *failed = Some(err.to_string());
            }
        }
        err
    }

    fn statement(&self, sql: &str) -> Result<Statement> {
        if let Some(s) = self.statements.borrow().get(sql) {
            return Ok(s.clone());
        }
        let translated = translate::pg_sql(sql)?;
        let st = wait(self.client.prepare(&translated)).map_err(|e| self.note(e))?;
        let mut cache = self.statements.borrow_mut();
        if cache.len() >= STATEMENTS {
            cache.clear();
        }
        cache.insert(sql.to_string(), st.clone());
        Ok(st)
    }

    /// Run `op` with the statement; once more after a stale cached plan.
    fn with_statement<T>(&self, sql: &str, mut op: impl FnMut(&Statement) -> std::result::Result<T, tokio_postgres::Error>) -> Result<T> {
        let mut retried = false;
        loop {
            let st = self.statement(sql)?;
            match op(&st) {
                Ok(v) => return Ok(v),
                Err(e) => {
                    let stale = matches!(e.code().map(|c| c.code()), Some("0A000" | "26000"));
                    if stale && !retried && !self.in_tx.get() {
                        self.statements.borrow_mut().clear();
                        retried = true;
                        continue;
                    }
                    return Err(self.note(e));
                }
            }
        }
    }

    pub(crate) fn execute(&self, sql: &str, values: &[ValueRef<'_>]) -> Result<u64> {
        let params: Vec<Param<'_>> = values.iter().map(|v| Param(*v)).collect();
        let refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
        self.with_statement(sql, |st| wait(self.client.execute(st, &refs)))
    }

    pub(crate) fn execute_many(&self, sql: &str, rows: &[Vec<ValueRef<'_>>]) -> Result<u64> {
        let mut total = 0;
        for chunk in rows.chunks(PIPELINE) {
            let params: Vec<Vec<Param<'_>>> = chunk.iter().map(|r| r.iter().map(|v| Param(*v)).collect()).collect();
            let refs: Vec<Vec<&(dyn ToSql + Sync)>> = params.iter().map(|r| r.iter().map(|p| p as &(dyn ToSql + Sync)).collect()).collect();
            total += self.with_statement(sql, |st| {
                let futs = refs.iter().map(|r| self.client.execute(st, r));
                wait(futures::future::try_join_all(futs)).map(|n| n.iter().sum::<u64>())
            })?;
        }
        Ok(total)
    }

    pub(crate) fn query(&self, sql: &str, values: &[ValueRef<'_>]) -> Result<Vec<tokio_postgres::Row>> {
        let params: Vec<Param<'_>> = values.iter().map(|v| Param(*v)).collect();
        let refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
        self.with_statement(sql, |st| wait(self.client.query(st, &refs)))
    }

    pub(crate) fn batch(&self, sql: &str) -> Result<()> {
        wait(self.client.batch_execute(sql)).map_err(|e| self.note(e))
    }
}

/// A parameter, encoded for whatever type Postgres inferred for it.
#[derive(Debug)]
struct Param<'a>(ValueRef<'a>);

fn is_text(t: &Type) -> bool {
    matches!(*t, Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN)
}

type BoxError = Box<dyn std::error::Error + Sync + Send>;

fn put_text(s: &str, out: &mut BytesMut) -> std::result::Result<IsNull, BoxError> {
    if s.contains('\0') {
        return Err("text can't contain NUL characters".into());
    }
    out.put_slice(s.as_bytes());
    Ok(IsNull::No)
}

impl ToSql for Param<'_> {
    fn to_sql(&self, ty: &Type, out: &mut BytesMut) -> std::result::Result<IsNull, BoxError> {
        match (self.0, ty) {
            (ValueRef::Null, _) => Ok(IsNull::Yes),
            (ValueRef::Int(i), &Type::INT8) => i.to_sql(ty, out),
            (ValueRef::Int(i), &Type::INT4) => i32::try_from(i)?.to_sql(ty, out),
            (ValueRef::Int(i), &Type::INT2) => i16::try_from(i)?.to_sql(ty, out),
            (ValueRef::Int(i), &Type::OID) => u32::try_from(i)?.to_sql(ty, out),
            (ValueRef::Int(i), &Type::BOOL) => (i != 0).to_sql(ty, out),
            (ValueRef::Int(i), &Type::FLOAT8) => (i as f64).to_sql(ty, out),
            (ValueRef::Int(i), &Type::FLOAT4) => (i as f32).to_sql(ty, out),
            (ValueRef::Int(i), &Type::NUMERIC) => {
                numeric_from_i64(i, out);
                Ok(IsNull::No)
            }
            (ValueRef::Int(i), t) if is_text(t) => put_text(&i.to_string(), out),
            (ValueRef::Real(r), &Type::FLOAT8) => r.to_sql(ty, out),
            (ValueRef::Real(r), &Type::FLOAT4) => (r as f32).to_sql(ty, out),
            (ValueRef::Real(r), t) if is_text(t) => put_text(&r.to_string(), out),
            (ValueRef::Text(s), t) if is_text(t) || *t == Type::JSON => put_text(s, out),
            (ValueRef::Text(s), &Type::JSONB) => {
                out.put_u8(1);
                put_text(s, out)
            }
            (ValueRef::Text(s), &Type::BYTEA) => {
                out.put_slice(s.as_bytes());
                Ok(IsNull::No)
            }
            (ValueRef::Blob(b), &Type::BYTEA) => {
                out.put_slice(b);
                Ok(IsNull::No)
            }
            (ValueRef::Blob(b), t) if is_text(t) => put_text(std::str::from_utf8(b)?, out),
            (v, t) => Err(format!("can't bind {v:?} as {t}").into()),
        }
    }

    fn accepts(_: &Type) -> bool {
        true
    }

    to_sql_checked!();
}

/// Any column, decoded into the common model.
struct Any<'a>(ValueRef<'a>);

impl<'a> FromSql<'a> for Any<'a> {
    fn from_sql(ty: &Type, raw: &'a [u8]) -> std::result::Result<Self, BoxError> {
        Ok(Any(match *ty {
            Type::BOOL => ValueRef::Int(bool::from_sql(ty, raw)? as i64),
            Type::INT2 => ValueRef::Int(i16::from_sql(ty, raw)? as i64),
            Type::INT4 => ValueRef::Int(i32::from_sql(ty, raw)? as i64),
            Type::INT8 => ValueRef::Int(i64::from_sql(ty, raw)?),
            Type::OID => ValueRef::Int(u32::from_sql(ty, raw)? as i64),
            Type::FLOAT4 => ValueRef::Real(f32::from_sql(ty, raw)? as f64),
            Type::FLOAT8 => ValueRef::Real(f64::from_sql(ty, raw)?),
            Type::NUMERIC => numeric(raw)?,
            Type::BYTEA => ValueRef::Blob(raw),
            Type::JSONB => ValueRef::Text(std::str::from_utf8(raw.get(1..).unwrap_or_default())?),
            ref t if is_text(t) || *t == Type::JSON => ValueRef::Text(std::str::from_utf8(raw)?),
            ref t => return Err(format!("unsupported column type {t}").into()),
        }))
    }

    fn accepts(_: &Type) -> bool {
        true
    }
}

pub(crate) fn column<T>(r: &tokio_postgres::Row, i: usize, f: impl FnOnce(ValueRef<'_>) -> Result<T>) -> Result<T> {
    if i >= r.len() {
        return Err(Error::Conversion(format!("no column {i} (the row has {})", r.len())));
    }
    let v: Option<Any<'_>> = r.try_get(i).map_err(|e| Error::Conversion(describe(&e)))?;
    f(v.map(|a| a.0).unwrap_or(ValueRef::Null))
}

/// Decode a binary NUMERIC: an integer when it has no fraction and fits, else a real.
fn numeric(raw: &[u8]) -> std::result::Result<ValueRef<'static>, BoxError> {
    let word = |i: usize| -> std::result::Result<i16, BoxError> {
        raw.get(i..i + 2).map(|b| i16::from_be_bytes([b[0], b[1]])).ok_or_else(|| "truncated NUMERIC".into())
    };
    let (ndigits, weight, sign, dscale) = (word(0)? as usize, word(2)? as i32, word(4)? as u16, word(6)?);
    if sign == 0xC000 {
        return Ok(ValueRef::Real(f64::NAN));
    }
    let digits: Vec<i64> = (0..ndigits).map(|k| word(8 + 2 * k).map(|d| d as i64)).collect::<std::result::Result<_, _>>()?;
    let neg = sign == 0x4000;
    let integral = digits.iter().enumerate().all(|(k, d)| weight - (k as i32) >= 0 || *d == 0);
    if dscale == 0 && integral && weight < 5 {
        let mut v: i128 = 0;
        for k in 0..=weight.max(-1) {
            v = v * 10_000 + digits.get(k as usize).copied().unwrap_or(0) as i128;
        }
        let v = if neg { -v } else { v };
        if let Ok(i) = i64::try_from(v) {
            return Ok(ValueRef::Int(i));
        }
    }
    let mut f = 0f64;
    for (k, d) in digits.iter().enumerate() {
        f += *d as f64 * 10_000f64.powi(weight - k as i32);
    }
    Ok(ValueRef::Real(if neg { -f } else { f }))
}

fn numeric_from_i64(i: i64, out: &mut BytesMut) {
    let mut n = i.unsigned_abs();
    let mut digits = Vec::new();
    while n > 0 {
        digits.push((n % 10_000) as i16);
        n /= 10_000;
    }
    digits.reverse();
    out.put_i16(digits.len() as i16);
    out.put_i16(digits.len().saturating_sub(1) as i16);
    out.put_u16(if i < 0 { 0x4000 } else { 0 });
    out.put_i16(0);
    for d in digits {
        out.put_i16(d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numerics_round_trip() {
        for i in [0i64, 1, -1, 9999, 10_000, 123_456_789, -987_654_321_000, i64::MAX, i64::MIN + 1] {
            let mut b = BytesMut::new();
            numeric_from_i64(i, &mut b);
            assert_eq!(numeric(&b).unwrap(), ValueRef::Int(i), "{i}");
        }
        // 12.5: one digit before the point (12), one after (5000), dscale 1.
        let raw = [0, 2, 0, 0, 0, 0, 0, 1, 0, 12, 0x13, 0x88];
        assert_eq!(numeric(&raw).unwrap(), ValueRef::Real(12.5));
    }

    #[test]
    fn schema_lock_keys_are_stable() {
        assert_eq!(schema_lock_key("code"), schema_lock_key("code"));
        assert_ne!(schema_lock_key("code"), schema_lock_key("code2"));
    }
}

//! roll code's metadata database: one API over SQLite (a single node) and Postgres (a cluster).
//!
//! SQL is written in SQLite's dialect and translated for Postgres ([`translate`]); schemas too,
//! so both backends come from one definition.
//!
//! - [`Db::read`] runs statements in autocommit mode, each seeing the latest commit, like
//!   SQLite's readers.
//! - [`Db::write`] runs a closure in one transaction. On SQLite it holds the single writer;
//!   on Postgres it's `SERIALIZABLE` and **re-run** when Postgres reports a serialization
//!   failure or a deadlock, or the connection drops before the commit. So the closure must
//!   only touch the database (no side effects outside it), and code keeps the guarantees it had
//!   with one writer: read-then-write inside a closure is atomic.
//! - A write is durable when `write` returns: Postgres commits synchronously, and SQLite group-
//!   commits an fsync of the WAL after the transaction, outside the writer's lock
//!   (`durable.rs`). If the connection drops during `COMMIT`, the pool asks Postgres on another
//!   connection what became of the transaction (`pg_xact_status`; its id comes back with
//!   `BEGIN`): committed is success, aborted is re-run. Only if Postgres can't be reached to
//!   find out (about 15 s) is it [`Error::CommitUnknown`], and the caller must look before
//!   retrying.
//! - Connections that die (a Postgres restart or failover, a killed backend) are replaced, and
//!   reads and uncommitted writes re-run, backing off for up to about 10 s. Idle connections
//!   opened before one was found dead are dropped rather than reused.
//!
//! The API is synchronous and callable from any thread, async tasks included (as rusqlite
//! was). Postgres connections are driven by a runtime of their own, so a caller blocking on one
//! never stalls the runtime that called it.

mod durable;
mod pg;
mod sqlite;
pub mod translate;
mod value;

pub use value::{FromValue, Params, ToValue, Value, ValueRef};

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("query returned no rows")]
    NoRows,
    /// A value couldn't be converted to or from a column or parameter.
    #[error("{0}")]
    Conversion(String),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("{}", pg::describe(.0))]
    Postgres(#[from] tokio_postgres::Error),
    /// The connection was lost while committing: the transaction may or may not have happened.
    #[error("the database connection was lost while committing; the write may or may not have happened: {0}")]
    CommitUnknown(String),
    #[error("timed out after {0:?} waiting for a database connection")]
    PoolTimeout(Duration),
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// The SQLSTATE (Postgres) of a database error.
    pub fn code(&self) -> Option<&str> {
        match self {
            Error::Postgres(e) => e.code().map(|c| c.code()),
            _ => None,
        }
    }

    /// A unique or primary-key constraint was violated.
    pub fn is_unique_violation(&self) -> bool {
        match self {
            Error::Sqlite(rusqlite::Error::SqliteFailure(e, _)) => {
                e.code == rusqlite::ErrorCode::ConstraintViolation
                    && matches!(e.extended_code, rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE | rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY)
            }
            Error::Postgres(_) => self.code() == Some("23505"),
            _ => false,
        }
    }

    /// Any constraint (unique, not null, check, foreign key) was violated.
    pub fn is_constraint_violation(&self) -> bool {
        match self {
            Error::Sqlite(rusqlite::Error::SqliteFailure(e, _)) => e.code == rusqlite::ErrorCode::ConstraintViolation,
            Error::Postgres(_) => self.code().is_some_and(|c| c.starts_with("23")),
            _ => false,
        }
    }
}

/// `query_row(…).optional()`: no row is `None`, not an error.
pub trait OptionalExtension<T> {
    fn optional(self) -> Result<Option<T>>;
}

impl<T> OptionalExtension<T> for Result<T> {
    fn optional(self) -> Result<Option<T>> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(Error::NoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Sqlite,
    Postgres,
}

/// Isolation of a write on Postgres. SQLite always has one writer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Isolation {
    /// The default: as if writes ran one at a time, re-run on conflicts.
    Serializable,
    /// For writes whose correctness rests on their own compare-and-swap (the lander's ref
    /// update), so unrelated readers can't make them retry.
    ReadCommitted,
}

/// Where a database lives.
#[derive(Clone, Debug)]
pub enum Target {
    Sqlite(PathBuf),
    /// A Postgres URL and the schema (namespace) the tables go in.
    Postgres { url: String, schema: String },
}

#[derive(Clone, Debug)]
pub struct SqliteOptions {
    /// Reader connections; reads don't queue behind the writer.
    pub readers: usize,
    /// Page cache of the writer, in KiB.
    pub cache_kib: i64,
}

impl Default for SqliteOptions {
    fn default() -> Self {
        SqliteOptions { readers: 4, cache_kib: 16 << 10 }
    }
}

#[derive(Clone, Debug)]
pub struct PgOptions {
    /// Connections this pool opens at most.
    pub max_connections: usize,
    /// CA certificates (PEM) to trust besides the system's, e.g. RDS's bundle.
    pub ca: Option<PathBuf>,
    /// Longest a statement may run before Postgres cancels it.
    pub statement_timeout: Duration,
    /// Longest a caller waits for a free connection.
    pub acquire_timeout: Duration,
}

impl Default for PgOptions {
    fn default() -> Self {
        PgOptions { max_connections: 16, ca: None, statement_timeout: Duration::from_secs(60), acquire_timeout: Duration::from_secs(30) }
    }
}

/// A database: a pool of connections. Cheap to clone.
#[derive(Clone)]
pub struct Db(Arc<Inner>);

enum Inner {
    Sqlite(Arc<sqlite::Sqlite>),
    Pg(pg::Pool),
}

/// The environment variable that points tests at Postgres instead of SQLite (see
/// [`Db::open_or_test`]). Never set in production: there the URL is configured explicitly.
pub const TEST_DATABASE_ENV: &str = "ROLL_CODE_TEST_DATABASE";

impl Db {
    pub fn open(target: &Target) -> Result<Db> {
        match target {
            Target::Sqlite(path) => Db::open_sqlite(path, SqliteOptions::default()),
            Target::Postgres { url, schema } => Db::connect(url, schema, PgOptions::default()),
        }
    }

    pub fn open_sqlite(path: &Path, opts: SqliteOptions) -> Result<Db> {
        Ok(Db(Arc::new(Inner::Sqlite(sqlite::Sqlite::open(path, &opts)?))))
    }

    /// Connect to Postgres (`postgres://user:pass@host:5432/db?sslmode=require`) and create
    /// `schema` if it doesn't exist. Connections are opened as they're needed.
    pub fn connect(url: &str, schema: &str, opts: PgOptions) -> Result<Db> {
        Ok(Db(Arc::new(Inner::Pg(pg::Pool::connect(url, schema, opts)?))))
    }

    /// `target` unless [`TEST_DATABASE_ENV`] names a Postgres URL and `target` is SQLite: then
    /// a fresh schema there, named after `name` and a random suffix. This is how the test suites
    /// run against Postgres unchanged.
    pub fn open_or_test(target: &Target, name: &str) -> Result<Db> {
        match (target, std::env::var(TEST_DATABASE_ENV)) {
            (Target::Sqlite(_), Ok(url)) if !url.is_empty() => {
                let schema = format!("t_{}_{name}", random_suffix());
                Db::connect(&url, &schema, PgOptions { max_connections: 8, ..PgOptions::default() })
            }
            _ => Db::open(target),
        }
    }

    pub fn backend(&self) -> Backend {
        match &*self.0 {
            Inner::Sqlite(_) => Backend::Sqlite,
            Inner::Pg(_) => Backend::Postgres,
        }
    }

    /// Run reads. Each statement sees the latest commit (no snapshot across statements).
    /// Re-run on Postgres if the connection drops.
    pub fn read<T, E: From<Error>>(&self, mut f: impl FnMut(&Conn<'_>) -> Result<T, E>) -> Result<T, E> {
        match &*self.0 {
            Inner::Sqlite(s) => s.read(|c| f(&Conn(ConnRef::Sqlite(c)))),
            Inner::Pg(p) => p.read(|c| f(&Conn(ConnRef::Pg(c)))),
        }
    }

    /// Run `f` in one serializable write transaction; durable when this returns. `f` may run
    /// more than once on Postgres (see the module docs), so it must only touch the database.
    pub fn write<T, E: From<Error>>(&self, f: impl FnMut(&Conn<'_>) -> Result<T, E>) -> Result<T, E> {
        self.write_with(Isolation::Serializable, f)
    }

    pub fn write_with<T, E: From<Error>>(&self, iso: Isolation, mut f: impl FnMut(&Conn<'_>) -> Result<T, E>) -> Result<T, E> {
        match &*self.0 {
            Inner::Sqlite(s) => s.write(true, |c| f(&Conn(ConnRef::Sqlite(c)))),
            Inner::Pg(p) => p.write(iso, |c| f(&Conn(ConnRef::Pg(c)))),
        }
    }

    /// [`Db::write`] without waiting for durability on SQLite (bulk work that calls
    /// [`Db::sync`] at the end). The same as `write` on Postgres.
    pub fn write_unsynced<T, E: From<Error>>(&self, mut f: impl FnMut(&Conn<'_>) -> Result<T, E>) -> Result<T, E> {
        match &*self.0 {
            Inner::Sqlite(s) => s.write(false, |c| f(&Conn(ConnRef::Sqlite(c)))),
            Inner::Pg(p) => p.write(Isolation::Serializable, |c| f(&Conn(ConnRef::Pg(c)))),
        }
    }

    /// Make every write committed so far durable (a no-op on Postgres).
    pub fn sync(&self) -> Result<()> {
        match &*self.0 {
            Inner::Sqlite(s) => s.sync(),
            Inner::Pg(_) => Ok(()),
        }
    }

    /// WAL syncs that reached the disk so far (SQLite; tests).
    pub fn syncs(&self) -> u64 {
        match &*self.0 {
            Inner::Sqlite(s) => s.wal.synced(),
            Inner::Pg(_) => 0,
        }
    }

    /// One statement in its own write transaction.
    pub fn execute(&self, sql: &str, params: impl Params) -> Result<usize> {
        let values: Vec<Value> = params.values()?.iter().map(|v| v.to_owned()).collect();
        self.write(|c| c.execute(sql, values.as_slice()))
    }

    pub fn query_row<T>(&self, sql: &str, params: impl Params, mut f: impl FnMut(&Row<'_>) -> Result<T>) -> Result<T> {
        self.read(|c| c.query_row(sql, &params, &mut f))
    }

    pub fn query<T>(&self, sql: &str, params: impl Params, mut f: impl FnMut(&Row<'_>) -> Result<T>) -> Result<Vec<T>> {
        self.read(|c| c.query(sql, &params, &mut f))
    }

    /// Create the tables and indexes of a SQLite schema (translated for Postgres) if they
    /// don't exist. On Postgres, every node may run this at once: it runs in one transaction
    /// under an advisory lock.
    pub fn ensure_schema(&self, ddl: &str) -> Result<()> {
        match &*self.0 {
            Inner::Sqlite(s) => s.write(true, |c| c.execute_batch(ddl).map_err(Error::from)),
            Inner::Pg(p) => p.ensure_schema(&translate::pg_ddl(ddl)?),
        }
    }

    /// Run `f` once per database for `name` (a migration), recording that it ran. Postgres runs
    /// it under the same lock as [`Db::ensure_schema`].
    pub fn migrate(&self, name: &str, mut f: impl FnMut(&Conn<'_>) -> Result<()>) -> Result<()> {
        const TABLE: &str = "CREATE TABLE IF NOT EXISTS code_migrations (name TEXT PRIMARY KEY, at INTEGER NOT NULL)";
        self.ensure_schema(TABLE)?;
        let mut run = |c: &Conn<'_>| -> Result<()> {
            if c.query_row("SELECT 1 FROM code_migrations WHERE name = ?", [name], |_| Ok(())).optional()?.is_some() {
                return Ok(());
            }
            f(c)?;
            c.execute("INSERT INTO code_migrations (name, at) VALUES (?, ?)", params![name, now_ms()])?;
            Ok(())
        };
        match &*self.0 {
            Inner::Sqlite(s) => s.write(true, |c| run(&Conn(ConnRef::Sqlite(c)))),
            Inner::Pg(p) => p.locked(|c| run(&Conn(ConnRef::Pg(c)))),
        }
    }

    /// Whether `table` has `column`.
    pub fn has_column(&self, table: &str, column: &str) -> Result<bool> {
        self.read(|c| c.has_column(table, column))
    }

    /// Add `column` (declared as in SQLite, e.g. `INTEGER NOT NULL DEFAULT 0`) to `table` unless
    /// it's there.
    pub fn add_column(&self, table: &str, column: &str, decl: &str) -> Result<()> {
        if self.has_column(table, column)? {
            return Ok(());
        }
        match &*self.0 {
            // Checked again under the writer's lock: another thread may have just added it.
            Inner::Sqlite(s) => s.write(true, |c| {
                let conn = Conn(ConnRef::Sqlite(c));
                if !conn.has_column(table, column)? {
                    conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"))?;
                }
                Ok(())
            }),
            Inner::Pg(p) => {
                let decl = translate::pg_ddl(&format!("CREATE TABLE x ({column} {decl}) WITHOUT ROWID"))?.remove(0);
                let decl = decl.trim_start_matches("CREATE TABLE x (").trim_end_matches(')');
                p.ensure_schema(&[format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS {decl}")])
            }
        }
    }

    /// The database's clock, in milliseconds since the epoch: one clock for every node.
    pub fn now_ms(&self) -> Result<i64> {
        match &*self.0 {
            Inner::Sqlite(_) => Ok(now_ms()),
            Inner::Pg(p) => p.read(|c| {
                Conn(ConnRef::Pg(c)).query_row("SELECT CAST(EXTRACT(EPOCH FROM clock_timestamp()) * 1000 AS BIGINT)", (), |r| r.get(0))
            }),
        }
    }

    /// The Postgres schema, if this is Postgres.
    pub fn pg_schema(&self) -> Option<&str> {
        match &*self.0 {
            Inner::Pg(p) => Some(p.schema()),
            Inner::Sqlite(_) => None,
        }
    }

    /// A wake-up for `NOTIFY channel` on Postgres: notified on each notification, and after the
    /// listening connection is re-established (anything may have been missed meanwhile).
    /// `None` on SQLite. Wake-ups are hints: readers must also poll.
    pub fn listen(&self, channel: &str) -> Option<tokio::sync::watch::Receiver<u64>> {
        match &*self.0 {
            Inner::Pg(p) => Some(p.listen(channel)),
            Inner::Sqlite(_) => None,
        }
    }
}

/// Wall-clock milliseconds since the epoch.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn random_suffix() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
    h.write_u32(std::process::id());
    format!("{:016x}", h.finish())
}

/// A connection, inside [`Db::read`] or a [`Db::write`] transaction.
pub struct Conn<'a>(ConnRef<'a>);

enum ConnRef<'a> {
    Sqlite(&'a rusqlite::Connection),
    Pg(&'a pg::PgConn),
}

impl Conn<'_> {
    pub fn backend(&self) -> Backend {
        match self.0 {
            ConnRef::Sqlite(_) => Backend::Sqlite,
            ConnRef::Pg(_) => Backend::Postgres,
        }
    }

    pub fn execute(&self, sql: &str, params: impl Params) -> Result<usize> {
        let values = params.values()?;
        match self.0 {
            ConnRef::Sqlite(c) => sqlite::execute(c, sql, &values),
            ConnRef::Pg(c) => c.execute(sql, &values).map(|n| n as usize),
        }
    }

    /// The same statement once per parameter list. Pipelined on Postgres (one round trip for
    /// the lot); returns the rows changed in total.
    pub fn execute_many<P: Params>(&self, sql: &str, rows: impl IntoIterator<Item = P>) -> Result<usize> {
        let rows: Vec<P> = rows.into_iter().collect();
        let values: Vec<Vec<ValueRef<'_>>> = rows.iter().map(|p| p.values()).collect::<Result<_>>()?;
        match self.0 {
            ConnRef::Sqlite(c) => {
                let mut n = 0;
                for v in &values {
                    n += sqlite::execute(c, sql, v)?;
                }
                Ok(n)
            }
            ConnRef::Pg(c) => c.execute_many(sql, &values).map(|n| n as usize),
        }
    }

    /// Statements without parameters, `;`-separated (schema changes).
    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        match self.0 {
            ConnRef::Sqlite(c) => Ok(c.execute_batch(sql)?),
            ConnRef::Pg(c) => c.batch(&translate::pg_sql(sql)?),
        }
    }

    /// The first row, mapped; [`Error::NoRows`] if there is none (see [`OptionalExtension`]).
    pub fn query_row<T>(&self, sql: &str, params: impl Params, mut f: impl FnMut(&Row<'_>) -> Result<T>) -> Result<T> {
        let values = params.values()?;
        match self.0 {
            ConnRef::Sqlite(c) => sqlite::query_row(c, sql, &values, &mut f),
            ConnRef::Pg(c) => {
                let rows = c.query(sql, &values)?;
                match rows.first() {
                    Some(r) => f(&Row(RowRef::Pg(r))),
                    None => Err(Error::NoRows),
                }
            }
        }
    }

    /// Every row, mapped.
    pub fn query<T>(&self, sql: &str, params: impl Params, mut f: impl FnMut(&Row<'_>) -> Result<T>) -> Result<Vec<T>> {
        let values = params.values()?;
        match self.0 {
            ConnRef::Sqlite(c) => sqlite::query(c, sql, &values, &mut f),
            ConnRef::Pg(c) => c.query(sql, &values)?.iter().map(|r| f(&Row(RowRef::Pg(r)))).collect(),
        }
    }

    /// A statement to run several times. Both backends cache prepared statements, so this is
    /// just a handle.
    pub fn prepare<'s>(&'s self, sql: &'s str) -> Result<Stmt<'s>> {
        Ok(Stmt { conn: self, sql })
    }

    pub fn prepare_cached<'s>(&'s self, sql: &'s str) -> Result<Stmt<'s>> {
        self.prepare(sql)
    }

    pub fn has_column(&self, table: &str, column: &str) -> Result<bool> {
        let sql = match self.0 {
            ConnRef::Sqlite(_) => "SELECT COUNT(*) FROM pragma_table_info(?) WHERE name = ?",
            ConnRef::Pg(_) => {
                "SELECT COUNT(*) FROM information_schema.columns WHERE table_schema = current_schema() AND table_name = ? AND column_name = ?"
            }
        };
        Ok(self.query_row(sql, [table, column], |r| r.get::<_, i64>(0))? > 0)
    }
}

/// A prepared statement (see [`Conn::prepare`]).
pub struct Stmt<'s> {
    conn: &'s Conn<'s>,
    sql: &'s str,
}

impl Stmt<'_> {
    pub fn execute(&mut self, params: impl Params) -> Result<usize> {
        self.conn.execute(self.sql, params)
    }

    pub fn query_row<T>(&mut self, params: impl Params, f: impl FnMut(&Row<'_>) -> Result<T>) -> Result<T> {
        self.conn.query_row(self.sql, params, f)
    }

    /// Every row, mapped (collected eagerly).
    pub fn query_map<T>(&mut self, params: impl Params, mut f: impl FnMut(&Row<'_>) -> Result<T>) -> Result<std::vec::IntoIter<Result<T>>> {
        let mut out = Vec::new();
        self.conn.query(self.sql, params, |r| {
            out.push(f(r));
            Ok(())
        })?;
        Ok(out.into_iter())
    }

    pub fn exists(&mut self, params: impl Params) -> Result<bool> {
        self.conn.query_row(self.sql, params, |_| Ok(())).optional().map(|r| r.is_some())
    }
}

/// A result row.
pub struct Row<'a>(RowRef<'a>);

enum RowRef<'a> {
    Sqlite(&'a rusqlite::Row<'a>),
    Pg(&'a tokio_postgres::Row),
}

/// A column index (rusqlite's `get::<_, T>(i)` shape).
pub trait RowIndex {
    fn index(&self) -> usize;
}

impl RowIndex for usize {
    fn index(&self) -> usize {
        *self
    }
}

impl Row<'_> {
    pub fn get<I: RowIndex, T: FromValue>(&self, idx: I) -> Result<T> {
        let i = idx.index();
        let r = match &self.0 {
            RowRef::Sqlite(r) => sqlite::column(r, i, T::from_value),
            RowRef::Pg(r) => pg::column(r, i, T::from_value),
        };
        r.map_err(|e| match e {
            Error::Conversion(m) => Error::Conversion(format!("column {i}: {m}")),
            e => e,
        })
    }
}

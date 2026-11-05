//! `gateway.db` (SQLite, mode 0600): client keys (only their SHA-256), usage roll-ups and the
//! request log. One connection behind a mutex; callers on the async side use
//! `spawn_blocking` or keep calls short (every statement here is indexed and small).
//!
//! - **Clients** (`clients.rs`): a key is `rgk_` + 32 random bytes (base64url); the row holds
//!   its SHA-256, never the key. [`Store::verify`] looks the hash up on every request
//!   (revocation is at once, even from another process: `roll gateway client revoke` beside a
//!   running `serve`); it touches `last_used_at` at most once a minute per client.
//! - **Usage** (`usage.rs`): one row per (hour, client, machine, project, thread, model asked
//!   for, upstream, upstream model), summed on insert (`ON CONFLICT … DO UPDATE`), so a busy
//!   gateway writes a bounded number of rows. Days are sums of hours. Kept `retention_days`.
//! - **Log** (`log.rs`): one row per request, the last 7 days and at most 100,000 rows; bodies
//!   ([`BODY_CAP`] each) in their own table, kept a day.
//!
//! Times are milliseconds since the epoch ([`agent_proto::Millis`]); buckets are UTC hours and
//! days. [`Store::prune`] is the caller's to run (hourly is plenty): nothing here runs on its
//! own.
//!
//! The store helper owns this module; the signatures below are the contract with `server` and
//! the daemon.

mod clients;
mod log;
mod usage;

use crate::{ClientRef, Record, Secret};
use agent_proto::{GatewayClient, LogBody, LogEntry, MachineId, ModelPrice, UsageBucket, UsageBy, UsageReport};
use anyhow::Context;
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

pub use log::{BODY_CAP, keep_body};

const HOUR: u64 = 3_600_000;
const DAY: u64 = 24 * HOUR;
/// The request log keeps this long, and at most [`LOG_ROWS`] rows.
const LOG_DAYS: u64 = 7;
const LOG_ROWS: u64 = 100_000;
/// Bodies are kept this long.
const BODY_DAYS: u64 = 1;

const MIGRATIONS: &[&str] = &[r"
    CREATE TABLE clients (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        machine TEXT,
        hash TEXT NOT NULL UNIQUE,
        created_at INTEGER NOT NULL,
        last_used_at INTEGER
    );
    CREATE INDEX clients_machine ON clients (machine);

    CREATE TABLE usage (
        hour INTEGER NOT NULL,
        client TEXT NOT NULL,
        machine TEXT NOT NULL,
        project TEXT NOT NULL,
        thread TEXT NOT NULL,
        model TEXT NOT NULL,
        upstream TEXT NOT NULL,
        upstream_model TEXT NOT NULL,
        requests INTEGER NOT NULL,
        errors INTEGER NOT NULL,
        input INTEGER NOT NULL,
        output INTEGER NOT NULL,
        cache_read INTEGER NOT NULL,
        cache_write INTEGER NOT NULL,
        cost INTEGER NOT NULL,
        PRIMARY KEY (hour, client, machine, project, thread, model, upstream, upstream_model)
    ) WITHOUT ROWID;

    CREATE TABLE log (
        seq INTEGER PRIMARY KEY AUTOINCREMENT,
        at INTEGER NOT NULL,
        client TEXT,
        machine TEXT,
        project TEXT,
        thread TEXT,
        format TEXT NOT NULL,
        model TEXT NOT NULL,
        stream INTEGER NOT NULL,
        status INTEGER NOT NULL,
        ms INTEGER NOT NULL,
        first_byte_ms INTEGER,
        attempts TEXT NOT NULL,
        input INTEGER NOT NULL,
        output INTEGER NOT NULL,
        cache_read INTEGER NOT NULL,
        cache_write INTEGER NOT NULL,
        cost INTEGER NOT NULL,
        error TEXT,
        failed INTEGER NOT NULL
    );
    CREATE INDEX log_at ON log (at);
    CREATE INDEX log_failed ON log (failed, seq);

    CREATE TABLE bodies (
        seq INTEGER PRIMARY KEY,
        at INTEGER NOT NULL,
        request TEXT NOT NULL,
        response TEXT NOT NULL,
        truncated INTEGER NOT NULL
    );
    CREATE INDEX bodies_at ON bodies (at);
"];

pub struct Store {
    db: Mutex<Connection>,
    /// Client id → when it was last seen, and when that was last written: `verify` writes
    /// `last_used_at` at most once a minute per client, [`Store::last_used`] reads both.
    seen: Mutex<HashMap<String, Seen>>,
}

#[derive(Clone, Copy)]
struct Seen {
    at: u64,
    written: u64,
}

impl Store {
    /// Opens (or makes) the database at `path`. A new file is made at mode 0600 before SQLite
    /// opens it, and an existing one is set to 0600 (SQLite gives its `-wal` and `-shm` files
    /// the database's mode).
    pub fn open(path: &Path) -> anyhow::Result<Store> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).with_context(|| format!("making {}", dir.display()))?;
        }
        private(path)?;
        let db = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "synchronous", "NORMAL")?;
        Store::new(db)
    }

    /// A store in memory (tests).
    pub fn memory() -> anyhow::Result<Store> {
        Store::new(Connection::open_in_memory()?)
    }

    fn new(mut db: Connection) -> anyhow::Result<Store> {
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        migrate(&mut db)?;
        Ok(Store { db: Mutex::new(db), seen: Mutex::new(HashMap::new()) })
    }

    fn db(&self) -> MutexGuard<'_, Connection> {
        // A panic while holding the lock can't leave SQLite half-written (transactions roll
        // back on drop), so a poisoned lock is still usable.
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn seen(&self) -> MutexGuard<'_, HashMap<String, Seen>> {
        self.seen.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Mints a client key; a client for the same machine is revoked first.
    pub fn issue(&self, name: &str, machine: Option<MachineId>, now: u64) -> anyhow::Result<(GatewayClient, Secret)> {
        clients::issue(self, name, machine, now)
    }

    /// The client `key` belongs to, if it's a live key.
    pub fn verify(&self, key: &str, now: u64) -> anyhow::Result<Option<ClientRef>> {
        clients::verify(self, key, now)
    }

    pub fn revoke(&self, id: &str) -> anyhow::Result<bool> {
        clients::revoke(self, id)
    }

    pub fn clients(&self) -> anyhow::Result<Vec<GatewayClient>> {
        clients::list(self)
    }

    /// (client id, last used) for every client that has been used.
    pub fn last_used(&self) -> anyhow::Result<Vec<(String, u64)>> {
        clients::last_used(self)
    }

    /// Adds a finished request to the usage roll-ups and the log; its cost from `prices` (the
    /// owner's) over the built-in list ([`crate::price`]). Bodies are kept only with
    /// `log_bodies`, each cut to [`BODY_CAP`] (`truncated` when one was longer).
    pub fn record(&self, record: &Record, prices: &[ModelPrice], log_bodies: bool) -> anyhow::Result<()> {
        let mut db = self.db();
        let tx = db.transaction()?;
        let cost = usage::add(&tx, record, prices)?;
        log::add(&tx, record, cost, log_bodies)?;
        tx.commit()?;
        Ok(())
    }

    pub fn usage(&self, from: u64, to: u64, by: UsageBy, bucket: UsageBucket) -> anyhow::Result<UsageReport> {
        usage::report(&self.db(), from, to, by, bucket)
    }

    /// A page of the log, newest first: with `after`, the newest entries with a greater `seq`
    /// (a live tail); with `before`, those with a smaller one (paging back); `limit` is capped
    /// at [`agent_proto::gateway_limits::LOG_PAGE`] (0: the cap). `errors`: only requests that
    /// failed (status 400 or more, or an error) or failed over (more than one attempt).
    pub fn log(&self, after: Option<u64>, before: Option<u64>, limit: u16, errors: bool) -> anyhow::Result<Vec<LogEntry>> {
        log::page(&self.db(), after, before, limit, errors)
    }

    pub fn log_body(&self, seq: u64) -> anyhow::Result<Option<LogBody>> {
        log::body(&self.db(), seq)
    }

    /// Drops usage older than `retention_days`, the log older than 7 days (and all but its
    /// newest 100,000 rows), bodies older than a day.
    pub fn prune(&self, now: u64, retention_days: u16) -> anyhow::Result<()> {
        let mut db = self.db();
        let tx = db.transaction()?;
        let usage_from = now.saturating_sub(u64::from(retention_days.max(1)) * DAY);
        tx.execute("DELETE FROM usage WHERE hour < ?1", [sql_int(usage_from)])?;
        tx.execute("DELETE FROM log WHERE at < ?1", [sql_int(now.saturating_sub(LOG_DAYS * DAY))])?;
        tx.execute("DELETE FROM log WHERE seq < (SELECT seq FROM log ORDER BY seq DESC LIMIT 1 OFFSET ?1)", [sql_int(LOG_ROWS - 1)])?;
        tx.execute(
            "DELETE FROM bodies WHERE at < ?1 OR NOT EXISTS (SELECT 1 FROM log WHERE log.seq = bodies.seq)",
            [sql_int(now.saturating_sub(BODY_DAYS * DAY))],
        )?;
        tx.commit()?;
        Ok(())
    }
}

/// Deletes the database at `path` with its `-wal` and `-shm` files (`Gateway::wipe`, after the
/// store is dropped). Files that aren't there are fine.
pub fn remove(path: &Path) -> anyhow::Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        let mut p = path.as_os_str().to_owned();
        p.push(suffix);
        match std::fs::remove_file(&p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", Path::new(&p).display())),
        }
    }
    Ok(())
}

/// Makes `path` if it isn't there, at 0600 from the start; sets an existing one to 0600.
fn private(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("making {}", path.display()))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).with_context(|| format!("making {} private", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn migrate(db: &mut Connection) -> anyhow::Result<()> {
    let version = db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))? as usize;
    if version > MIGRATIONS.len() {
        anyhow::bail!("gateway.db is from a newer roll (schema {version}, this one knows {})", MIGRATIONS.len());
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(version) {
        let tx = db.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", (i + 1) as i64)?;
        tx.commit()?;
    }
    Ok(())
}

/// SQLite's integers are signed: counts and times past `i64::MAX` (never in practice) stay there.
fn sql_int(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn from_sql(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

/// `s` cut to at most `max` bytes on a character boundary.
fn cut(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

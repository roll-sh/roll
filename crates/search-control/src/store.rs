//! Control-plane state for roll search: SQLite (`search.db`, one node) or Postgres (a fleet of
//! nodes sharing one catalog, the same database as the ownership leases and node records).
//!
//! - `clusters`: one row per cluster (name, dialect, password, limits, branch origin) plus the
//!   last figures metering saw (indices, documents, stored bytes), so listing clusters never
//!   wakes them or touches object storage. Every change takes a new `seq` from one counter, and
//!   a deleted cluster leaves a tombstone with its own: nodes of a fleet follow
//!   [`Store::changes_since`] to apply changes made through another node to the clusters they
//!   hold. A cluster being deleted keeps its row, flagged `deleting`, until its data is gone;
//!   lookups don't see it.
//! - `usage`: one row per metering interval of a *loaded* cluster: loaded seconds, the
//!   estimated resident bytes, requests. An idle cluster adds no rows.
//! - `storage`: a change log of each cluster's stored bytes. Storage accrues while a cluster
//!   sleeps too, so instead of a row per cluster per minute the log gets a row only when the
//!   size changes, and GB-months are the integral of that step function over the window.
//!
//! SQLite migrations are append-only and tracked with `PRAGMA user_version`; the Postgres
//! schema is created idempotently under an advisory lock (nodes start together).
//!
//! Calls are async: SQLite statements run inline (microseconds on a local file, as the other
//! control planes do), Postgres ones await the database over one reconnecting connection.

use rusqlite::{Connection, OptionalExtension, Row, params};
use search_proxy::pg::Pg;
use serde::Serialize;
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, Serialize)]
pub struct SearchCluster {
    pub id: String,
    /// The tenant's name on the node: the Basic auth user, the host label under the node's
    /// domain, `GET /` `cluster_name`, and its key in object storage. Immutable.
    pub name: String,
    pub region: String,
    /// `elasticsearch` or `opensearch`: the REST dialect the cluster speaks.
    pub flavor: String,
    #[serde(skip)]
    pub password: String,
    /// Seconds without requests before the cluster is unloaded (scale to zero).
    pub idle_timeout_s: i64,
    /// Requests running at once (as many more may queue; beyond that, 429).
    pub max_in_flight: i64,
    /// `http.max_content_length`, in MiB.
    pub max_body_mb: i64,
    /// Default of `action.destructive_requires_name`.
    pub destructive_requires_name: bool,
    pub parent_id: Option<String>,
    /// Unix seconds of the parent's state the branch started from.
    pub parent_at: Option<i64>,
    pub created_at: i64,
    pub password_rotated_at: Option<i64>,
    /// What metering last saw (it refreshes them while the cluster is loaded and once after it
    /// unloads).
    pub indices: i64,
    pub docs: i64,
    pub stored_bytes: i64,
}

/// One metering interval of a loaded cluster.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageSample {
    pub at: i64,
    /// Seconds the cluster was loaded during the interval.
    pub seconds: i64,
    /// Estimated resident bytes while loaded.
    pub loaded_bytes: i64,
    pub requests: i64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct UsageTotals {
    /// Seconds the cluster was loaded (in memory).
    pub loaded_seconds: i64,
    /// Integral of the resident size over loaded time.
    pub loaded_gib_hours: f64,
    pub requests: i64,
    pub peak_loaded_bytes: i64,
    /// Integral of the stored size over the window.
    pub stored_gib_months: f64,
}

enum Db {
    Sqlite(Mutex<Connection>),
    Pg(Arc<Pg>),
}

pub struct Store {
    db: Db,
}

const MIGRATIONS: &[&str] = &[
    r#"
CREATE TABLE search_clusters (
  id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, region TEXT NOT NULL, flavor TEXT NOT NULL,
  password TEXT NOT NULL, idle_timeout_s INTEGER NOT NULL, max_in_flight INTEGER NOT NULL,
  max_body_mb INTEGER NOT NULL, destructive_requires_name INTEGER NOT NULL,
  parent_id TEXT, parent_at INTEGER, created_at INTEGER NOT NULL, password_rotated_at INTEGER,
  indices INTEGER NOT NULL DEFAULT 0, docs INTEGER NOT NULL DEFAULT 0, stored_bytes INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE search_usage (
  cluster_id TEXT NOT NULL, at INTEGER NOT NULL, seconds INTEGER NOT NULL, loaded_bytes INTEGER NOT NULL,
  requests INTEGER NOT NULL
);
CREATE INDEX search_usage_cluster ON search_usage (cluster_id, at);
CREATE TABLE search_storage (cluster_id TEXT NOT NULL, at INTEGER NOT NULL, bytes INTEGER NOT NULL);
CREATE INDEX search_storage_cluster ON search_storage (cluster_id, at);
"#,
    r#"
ALTER TABLE search_clusters ADD COLUMN seq INTEGER NOT NULL DEFAULT 0;
ALTER TABLE search_clusters ADD COLUMN deleting INTEGER NOT NULL DEFAULT 0;
CREATE INDEX search_clusters_seq ON search_clusters (seq);
CREATE TABLE search_tombstones (name TEXT NOT NULL, seq INTEGER NOT NULL, at INTEGER NOT NULL);
CREATE INDEX search_tombstones_seq ON search_tombstones (seq);
"#,
    // Nodes look up the always-on clusters every few lease periods; few clusters are.
    r#"
CREATE INDEX search_clusters_always_on ON search_clusters (created_at) WHERE idle_timeout_s = 0;
"#,
];

const PG_SCHEMA: &str = r#"
CREATE SEQUENCE IF NOT EXISTS roll_search_seq;
CREATE TABLE IF NOT EXISTS roll_search_clusters (
  id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, region TEXT NOT NULL, flavor TEXT NOT NULL,
  password TEXT NOT NULL, idle_timeout_s BIGINT NOT NULL, max_in_flight BIGINT NOT NULL,
  max_body_mb BIGINT NOT NULL, destructive_requires_name BOOLEAN NOT NULL,
  parent_id TEXT, parent_at BIGINT, created_at BIGINT NOT NULL, password_rotated_at BIGINT,
  indices BIGINT NOT NULL DEFAULT 0, docs BIGINT NOT NULL DEFAULT 0, stored_bytes BIGINT NOT NULL DEFAULT 0,
  seq BIGINT NOT NULL DEFAULT nextval('roll_search_seq'), deleting BOOLEAN NOT NULL DEFAULT false
);
CREATE INDEX IF NOT EXISTS roll_search_clusters_seq ON roll_search_clusters (seq);
CREATE INDEX IF NOT EXISTS roll_search_clusters_parent ON roll_search_clusters (parent_id);
CREATE INDEX IF NOT EXISTS roll_search_clusters_always_on ON roll_search_clusters (created_at) WHERE idle_timeout_s = 0;
CREATE TABLE IF NOT EXISTS roll_search_tombstones (name TEXT NOT NULL, seq BIGINT NOT NULL, at BIGINT NOT NULL);
CREATE INDEX IF NOT EXISTS roll_search_tombstones_seq ON roll_search_tombstones (seq);
CREATE TABLE IF NOT EXISTS roll_search_usage (
  cluster_id TEXT NOT NULL, at BIGINT NOT NULL, seconds BIGINT NOT NULL, loaded_bytes BIGINT NOT NULL, requests BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS roll_search_usage_cluster ON roll_search_usage (cluster_id, at);
CREATE TABLE IF NOT EXISTS roll_search_storage (cluster_id TEXT NOT NULL, at BIGINT NOT NULL, bytes BIGINT NOT NULL);
CREATE INDEX IF NOT EXISTS roll_search_storage_cluster ON roll_search_storage (cluster_id, at);
"#;

/// Advisory lock key serializing schema creation across nodes (`roll_search` in ASCII).
const PG_SCHEMA_LOCK: i64 = 0x726f_6c6c_5f73_6561;

const COLS: &str = "id, name, region, flavor, password, idle_timeout_s, max_in_flight, max_body_mb, destructive_requires_name, \
                    parent_id, parent_at, created_at, password_rotated_at, indices, docs, stored_bytes";

/// Tombstones older than this are dropped (a node that hasn't followed for a day reloads).
const TOMBSTONE_KEEP_S: i64 = 86_400;

fn row(r: &Row) -> rusqlite::Result<SearchCluster> {
    Ok(SearchCluster {
        id: r.get(0)?,
        name: r.get(1)?,
        region: r.get(2)?,
        flavor: r.get(3)?,
        password: r.get(4)?,
        idle_timeout_s: r.get(5)?,
        max_in_flight: r.get(6)?,
        max_body_mb: r.get(7)?,
        destructive_requires_name: r.get(8)?,
        parent_id: r.get(9)?,
        parent_at: r.get(10)?,
        created_at: r.get(11)?,
        password_rotated_at: r.get(12)?,
        indices: r.get(13)?,
        docs: r.get(14)?,
        stored_bytes: r.get(15)?,
    })
}

fn pg_row(r: &tokio_postgres::Row) -> SearchCluster {
    SearchCluster {
        id: r.get(0),
        name: r.get(1),
        region: r.get(2),
        flavor: r.get(3),
        password: r.get(4),
        idle_timeout_s: r.get(5),
        max_in_flight: r.get(6),
        max_body_mb: r.get(7),
        destructive_requires_name: r.get(8),
        parent_id: r.get(9),
        parent_at: r.get(10),
        created_at: r.get(11),
        password_rotated_at: r.get(12),
        indices: r.get(13),
        docs: r.get(14),
        stored_bytes: r.get(15),
    }
}

/// Seconds in the 30-day month the GB-month figures use (AWS bills storage per 730-hour month;
/// a 30-day month keeps `days / 30` exact in the dashboard).
pub const MONTH_SECONDS: f64 = 30.0 * 86_400.0;
const GIB: f64 = (1u64 << 30) as f64;

/// The next value of the change counter, inside a SQLite statement.
const SQLITE_NEXT_SEQ: &str =
    "(SELECT MAX(s) + 1 FROM (SELECT COALESCE(MAX(seq), 0) AS s FROM search_clusters UNION ALL SELECT COALESCE(MAX(seq), 0) FROM search_tombstones))";

fn is_unique_violation(e: &(dyn std::error::Error + 'static)) -> bool {
    if let Some(e) = e.downcast_ref::<rusqlite::Error>() {
        return matches!(e, rusqlite::Error::SqliteFailure(f, _) if f.code == rusqlite::ErrorCode::ConstraintViolation);
    }
    if let Some(e) = e.downcast_ref::<tokio_postgres::Error>() {
        return e.code() == Some(&tokio_postgres::error::SqlState::UNIQUE_VIOLATION);
    }
    false
}

fn now_s() -> i64 {
    crate::meter::now_ms() / 1000
}

/// SQLite refused because another connection holds the lock it needs.
fn is_busy(e: &rusqlite::Error) -> bool {
    matches!(e.sqlite_error_code(), Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
}

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        // Nodes of one host share the file and may start together. SQLite skips the busy handler
        // where waiting could deadlock (switching a fresh file to WAL, upgrading a read lock), so
        // a contended open is retried as a whole for a while.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut pause = std::time::Duration::from_millis(5);
        loop {
            match Self::open_once(path) {
                Ok(conn) => return Ok(Self { db: Db::Sqlite(Mutex::new(conn)) }),
                Err(e) if is_busy(&e) && std::time::Instant::now() < deadline => {
                    std::thread::sleep(pause);
                    pause = (pause * 2).min(std::time::Duration::from_millis(200));
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    fn open_once(path: &Path) -> rusqlite::Result<Connection> {
        let mut conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA busy_timeout = 5000; PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;
        // Each migration step reads the version inside a write transaction, so a step runs once,
        // whoever gets there first.
        loop {
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let version: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            let Some(sql) = MIGRATIONS.get(version.max(0) as usize) else { break };
            tx.execute_batch(sql)?;
            tx.execute_batch(&format!("PRAGMA user_version = {}", version + 1))?;
            tx.commit()?;
        }
        Ok(conn)
    }

    pub fn memory() -> anyhow::Result<Self> {
        Self::open(Path::new(":memory:"))
    }

    /// The catalog in Postgres, shared by every node of a fleet.
    pub async fn postgres(pg: Arc<Pg>) -> anyhow::Result<Self> {
        {
            let mut c = pg.client().await?;
            let tx = c.transaction().await?;
            tx.execute("SELECT pg_advisory_xact_lock($1)", &[&PG_SCHEMA_LOCK]).await?;
            tx.batch_execute(PG_SCHEMA).await?;
            tx.commit().await?;
        }
        Ok(Self { db: Db::Pg(pg) })
    }

    /// Whether several nodes share this catalog (changes must be followed).
    pub fn is_shared(&self) -> bool {
        matches!(self.db, Db::Pg(_))
    }

    fn with<T>(conn: &Mutex<Connection>, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> anyhow::Result<T> {
        // A panic while holding the lock can't leave SQLite inconsistent (statements are atomic).
        let conn = conn.lock().unwrap_or_else(|e| e.into_inner());
        Ok(f(&conn)?)
    }

    /// Inserts a new cluster; `false` when its name (or id) is taken.
    pub async fn insert(&self, c: &SearchCluster) -> anyhow::Result<bool> {
        let r = match &self.db {
            Db::Sqlite(conn) => Self::with(conn, |x| {
                x.execute(
                    &format!(
                        "INSERT INTO search_clusters ({COLS}, seq) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, {SQLITE_NEXT_SEQ})"
                    ),
                    params![
                        c.id,
                        c.name,
                        c.region,
                        c.flavor,
                        c.password,
                        c.idle_timeout_s,
                        c.max_in_flight,
                        c.max_body_mb,
                        c.destructive_requires_name,
                        c.parent_id,
                        c.parent_at,
                        c.created_at,
                        c.password_rotated_at,
                        c.indices,
                        c.docs,
                        c.stored_bytes
                    ],
                )
                .map(|_| ())
            }),
            Db::Pg(pg) => pg
                .client()
                .await?
                .execute(
                    &format!("INSERT INTO roll_search_clusters ({COLS}) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)"),
                    &[
                        &c.id,
                        &c.name,
                        &c.region,
                        &c.flavor,
                        &c.password,
                        &c.idle_timeout_s,
                        &c.max_in_flight,
                        &c.max_body_mb,
                        &c.destructive_requires_name,
                        &c.parent_id,
                        &c.parent_at,
                        &c.created_at,
                        &c.password_rotated_at,
                        &c.indices,
                        &c.docs,
                        &c.stored_bytes,
                    ],
                )
                .await
                .map(|_| ())
                .map_err(anyhow::Error::from),
        };
        match r {
            Ok(()) => Ok(true),
            Err(e) if e.chain().any(is_unique_violation) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Saves the settings and the password (not the metering figures: see [`Store::set_stats`]).
    pub async fn update(&self, c: &SearchCluster) -> anyhow::Result<()> {
        match &self.db {
            Db::Sqlite(conn) => Self::with(conn, |x| {
                x.execute(
                    &format!(
                        "UPDATE search_clusters SET flavor = ?2, password = ?3, idle_timeout_s = ?4, max_in_flight = ?5, max_body_mb = ?6,
                         destructive_requires_name = ?7, password_rotated_at = ?8, seq = {SQLITE_NEXT_SEQ} WHERE id = ?1"
                    ),
                    params![c.id, c.flavor, c.password, c.idle_timeout_s, c.max_in_flight, c.max_body_mb, c.destructive_requires_name, c.password_rotated_at],
                )
                .map(|_| ())
            }),
            Db::Pg(pg) => {
                pg.client()
                    .await?
                    .execute(
                        "UPDATE roll_search_clusters SET flavor = $2, password = $3, idle_timeout_s = $4, max_in_flight = $5, max_body_mb = $6,
                         destructive_requires_name = $7, password_rotated_at = $8, seq = nextval('roll_search_seq') WHERE id = $1",
                        &[&c.id, &c.flavor, &c.password, &c.idle_timeout_s, &c.max_in_flight, &c.max_body_mb, &c.destructive_requires_name, &c.password_rotated_at],
                    )
                    .await?;
                Ok(())
            }
        }
    }

    async fn one(&self, filter: &str, arg: &str) -> anyhow::Result<Option<SearchCluster>> {
        match &self.db {
            Db::Sqlite(conn) => Self::with(conn, |x| x.query_row(&format!("SELECT {COLS} FROM search_clusters WHERE {filter} = ?1 AND deleting = 0"), [arg], row).optional()),
            Db::Pg(pg) => Ok(pg
                .client()
                .await?
                .query_opt(&format!("SELECT {COLS} FROM roll_search_clusters WHERE {filter} = $1 AND NOT deleting"), &[&arg])
                .await?
                .map(|r| pg_row(&r))),
        }
    }

    pub async fn get(&self, id: &str) -> anyhow::Result<Option<SearchCluster>> {
        self.one("id", id).await
    }

    /// The cluster named `name`, unless it is being deleted.
    pub async fn by_name(&self, name: &str) -> anyhow::Result<Option<SearchCluster>> {
        self.one("name", name).await
    }

    async fn many(&self, filter: &str, arg: Option<&str>) -> anyhow::Result<Vec<SearchCluster>> {
        match &self.db {
            Db::Sqlite(conn) => Self::with(conn, |x| {
                let mut st = x.prepare(&format!("SELECT {COLS} FROM search_clusters WHERE deleting = 0 {filter} ORDER BY created_at, rowid"))?;
                match arg {
                    Some(a) => st.query_map([a], row)?.collect(),
                    None => st.query_map([], row)?.collect(),
                }
            }),
            Db::Pg(pg) => {
                let c = pg.client().await?;
                let sql = format!("SELECT {COLS} FROM roll_search_clusters WHERE NOT deleting {} ORDER BY created_at, id", filter.replace("?1", "$1"));
                let rows = match arg {
                    Some(a) => c.query(&sql, &[&a]).await?,
                    None => c.query(&sql, &[]).await?,
                };
                Ok(rows.iter().map(pg_row).collect())
            }
        }
    }

    pub async fn list(&self) -> anyhow::Result<Vec<SearchCluster>> {
        self.many("", None).await
    }

    /// Clusters that never unload (`idle_timeout_s` 0), through a partial index: what every node
    /// of a fleet polls, so it stays cheap however large the catalog grows.
    pub async fn always_on(&self) -> anyhow::Result<Vec<SearchCluster>> {
        self.many("AND idle_timeout_s = 0", None).await
    }

    /// Clusters branched from `id`.
    pub async fn children(&self, id: &str) -> anyhow::Result<Vec<SearchCluster>> {
        self.many("AND parent_id = ?1", Some(id)).await
    }

    /// Hides a cluster from lookups while its data is deleted (`on`), or shows it again (the
    /// delete failed). Bumps its change counter, so other nodes notice.
    pub async fn set_deleting(&self, id: &str, on: bool) -> anyhow::Result<()> {
        match &self.db {
            Db::Sqlite(conn) => Self::with(conn, |x| {
                x.execute(&format!("UPDATE search_clusters SET deleting = ?2, seq = {SQLITE_NEXT_SEQ} WHERE id = ?1"), params![id, on]).map(|_| ())
            }),
            Db::Pg(pg) => {
                pg.client().await?.execute("UPDATE roll_search_clusters SET deleting = $2, seq = nextval('roll_search_seq') WHERE id = $1", &[&id, &on]).await?;
                Ok(())
            }
        }
    }

    /// Deletes a cluster's rows and leaves a tombstone for followers.
    pub async fn delete(&self, id: &str) -> anyhow::Result<()> {
        let now = now_s();
        match &self.db {
            Db::Sqlite(conn) => Self::with(conn, |x| {
                let name: Option<String> = x.query_row("SELECT name FROM search_clusters WHERE id = ?1", [id], |r| r.get(0)).optional()?;
                if let Some(name) = &name {
                    x.execute(&format!("INSERT INTO search_tombstones (name, seq, at) VALUES (?1, {SQLITE_NEXT_SEQ}, ?2)"), params![name, now])?;
                }
                x.execute("DELETE FROM search_clusters WHERE id = ?1", [id])?;
                x.execute("DELETE FROM search_usage WHERE cluster_id = ?1", [id])?;
                x.execute("DELETE FROM search_storage WHERE cluster_id = ?1", [id])?;
                x.execute("DELETE FROM search_tombstones WHERE at < ?1", [now - TOMBSTONE_KEEP_S]).map(|_| ())
            }),
            Db::Pg(pg) => {
                let mut c = pg.client().await?;
                let tx = c.transaction().await?;
                tx.execute(
                    "INSERT INTO roll_search_tombstones (name, seq, at) SELECT name, nextval('roll_search_seq'), $2 FROM roll_search_clusters WHERE id = $1",
                    &[&id, &now],
                )
                .await?;
                tx.execute("DELETE FROM roll_search_clusters WHERE id = $1", &[&id]).await?;
                tx.execute("DELETE FROM roll_search_usage WHERE cluster_id = $1", &[&id]).await?;
                tx.execute("DELETE FROM roll_search_storage WHERE cluster_id = $1", &[&id]).await?;
                tx.execute("DELETE FROM roll_search_tombstones WHERE at < $1", &[&(now - TOMBSTONE_KEEP_S)]).await?;
                tx.commit().await?;
                Ok(())
            }
        }
    }

    /// Names of clusters changed (created, updated, being deleted, deleted) after `seq`, with
    /// the newest counter value seen. Metering figures don't count as changes.
    pub async fn changes_since(&self, seq: i64) -> anyhow::Result<(Vec<String>, i64)> {
        let rows: Vec<(String, i64)> = match &self.db {
            Db::Sqlite(conn) => Self::with(conn, |x| {
                let mut st = x.prepare_cached("SELECT name, seq FROM search_clusters WHERE seq > ?1 UNION ALL SELECT name, seq FROM search_tombstones WHERE seq > ?1")?;
                st.query_map([seq], |r| Ok((r.get(0)?, r.get(1)?)))?.collect()
            })?,
            Db::Pg(pg) => pg
                .client()
                .await?
                .query("SELECT name, seq FROM roll_search_clusters WHERE seq > $1 UNION ALL SELECT name, seq FROM roll_search_tombstones WHERE seq > $1", &[&seq])
                .await?
                .iter()
                .map(|r| (r.get(0), r.get(1)))
                .collect(),
        };
        let top = rows.iter().map(|(_, s)| *s).max().unwrap_or(seq).max(seq);
        let mut names: Vec<String> = rows.into_iter().map(|(n, _)| n).collect();
        names.sort();
        names.dedup();
        Ok((names, top))
    }

    /// The change counter now (a follower starts from here).
    pub async fn current_seq(&self) -> anyhow::Result<i64> {
        Ok(match &self.db {
            Db::Sqlite(conn) => Self::with(conn, |x| {
                x.query_row("SELECT MAX(s) FROM (SELECT COALESCE(MAX(seq), 0) AS s FROM search_clusters UNION ALL SELECT COALESCE(MAX(seq), 0) FROM search_tombstones)", [], |r| r.get(0))
            })?,
            Db::Pg(pg) => pg
                .client()
                .await?
                .query_one("SELECT GREATEST((SELECT COALESCE(MAX(seq), 0) FROM roll_search_clusters), (SELECT COALESCE(MAX(seq), 0) FROM roll_search_tombstones))", &[])
                .await?
                .get(0),
        })
    }

    /// The latest figures metering saw. A change of `stored_bytes` is logged at `at` for the
    /// storage integral.
    pub async fn set_stats(&self, id: &str, at: i64, indices: i64, docs: i64, stored_bytes: i64) -> anyhow::Result<()> {
        match &self.db {
            Db::Sqlite(conn) => Self::with(conn, |x| {
                x.execute("UPDATE search_clusters SET indices = ?2, docs = ?3, stored_bytes = ?4 WHERE id = ?1", params![id, indices, docs, stored_bytes])?;
                let last: Option<i64> = x
                    .query_row("SELECT bytes FROM search_storage WHERE cluster_id = ?1 ORDER BY at DESC, rowid DESC LIMIT 1", [id], |r| r.get(0))
                    .optional()?;
                if last != Some(stored_bytes) {
                    x.execute("INSERT INTO search_storage (cluster_id, at, bytes) VALUES (?1, ?2, ?3)", params![id, at, stored_bytes])?;
                }
                Ok(())
            }),
            Db::Pg(pg) => {
                let mut c = pg.client().await?;
                let tx = c.transaction().await?;
                tx.execute("UPDATE roll_search_clusters SET indices = $2, docs = $3, stored_bytes = $4 WHERE id = $1", &[&id, &indices, &docs, &stored_bytes]).await?;
                // The UPDATE locked the cluster's row: nodes metering it at once (a handoff)
                // take turns, so the change log gets one row per change.
                let last: Option<i64> = tx
                    .query_opt("SELECT bytes FROM roll_search_storage WHERE cluster_id = $1 ORDER BY at DESC, ctid DESC LIMIT 1", &[&id])
                    .await?
                    .map(|r| r.get(0));
                if last != Some(stored_bytes) {
                    tx.execute("INSERT INTO roll_search_storage (cluster_id, at, bytes) VALUES ($1, $2, $3)", &[&id, &at, &stored_bytes]).await?;
                }
                tx.commit().await?;
                Ok(())
            }
        }
    }

    pub async fn record_usage(&self, id: &str, u: &UsageSample) -> anyhow::Result<()> {
        match &self.db {
            Db::Sqlite(conn) => Self::with(conn, |x| {
                x.execute(
                    "INSERT INTO search_usage (cluster_id, at, seconds, loaded_bytes, requests) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![id, u.at, u.seconds, u.loaded_bytes, u.requests],
                )
                .map(|_| ())
            }),
            Db::Pg(pg) => {
                pg.client()
                    .await?
                    .execute(
                        "INSERT INTO roll_search_usage (cluster_id, at, seconds, loaded_bytes, requests) VALUES ($1, $2, $3, $4, $5)",
                        &[&id, &u.at, &u.seconds, &u.loaded_bytes, &u.requests],
                    )
                    .await?;
                Ok(())
            }
        }
    }

    /// Totals over `[since, now]`.
    pub async fn usage_since(&self, id: &str, since: i64, now: i64) -> anyhow::Result<UsageTotals> {
        let (seconds, byte_seconds, requests, peak): (i64, f64, i64, i64) = match &self.db {
            Db::Sqlite(conn) => Self::with(conn, |x| {
                x.query_row(
                    "SELECT COALESCE(SUM(seconds), 0), COALESCE(SUM(CAST(seconds AS REAL) * loaded_bytes), 0.0), COALESCE(SUM(requests), 0),
                            COALESCE(MAX(loaded_bytes), 0)
                     FROM search_usage WHERE cluster_id = ?1 AND at >= ?2",
                    params![id, since],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
            })?,
            Db::Pg(pg) => {
                let r = pg
                    .client()
                    .await?
                    .query_one(
                        "SELECT COALESCE(SUM(seconds), 0)::BIGINT, COALESCE(SUM(seconds::FLOAT8 * loaded_bytes), 0)::FLOAT8,
                                COALESCE(SUM(requests), 0)::BIGINT, COALESCE(MAX(loaded_bytes), 0)::BIGINT
                         FROM roll_search_usage WHERE cluster_id = $1 AND at >= $2",
                        &[&id, &since],
                    )
                    .await?;
                (r.get(0), r.get(1), r.get(2), r.get(3))
            }
        };
        Ok(UsageTotals {
            loaded_seconds: seconds,
            loaded_gib_hours: byte_seconds / GIB / 3600.0,
            requests,
            peak_loaded_bytes: peak,
            stored_gib_months: self.storage_byte_seconds(id, since, now).await? / GIB / MONTH_SECONDS,
        })
    }

    /// The integral of stored bytes over `[since, now]`: the size in effect at `since` (the last
    /// change before it) and every change after.
    pub async fn storage_byte_seconds(&self, id: &str, since: i64, now: i64) -> anyhow::Result<f64> {
        let points: Vec<(i64, i64)> = match &self.db {
            Db::Sqlite(conn) => Self::with(conn, |x| {
                let before: Option<(i64, i64)> = x
                    .query_row(
                        "SELECT at, bytes FROM search_storage WHERE cluster_id = ?1 AND at < ?2 ORDER BY at DESC, rowid DESC LIMIT 1",
                        params![id, since],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                let mut st = x.prepare("SELECT at, bytes FROM search_storage WHERE cluster_id = ?1 AND at >= ?2 AND at <= ?3 ORDER BY at, rowid")?;
                let mut v: Vec<(i64, i64)> = before.map(|(_, b)| (since, b)).into_iter().collect();
                for p in st.query_map(params![id, since, now], |r| Ok((r.get(0)?, r.get(1)?)))? {
                    v.push(p?);
                }
                Ok(v)
            })?,
            Db::Pg(pg) => {
                let c = pg.client().await?;
                let before = c
                    .query_opt("SELECT bytes FROM roll_search_storage WHERE cluster_id = $1 AND at < $2 ORDER BY at DESC, ctid DESC LIMIT 1", &[&id, &since])
                    .await?;
                let mut v: Vec<(i64, i64)> = before.map(|r| (since, r.get(0))).into_iter().collect();
                for r in c.query("SELECT at, bytes FROM roll_search_storage WHERE cluster_id = $1 AND at >= $2 AND at <= $3 ORDER BY at, ctid", &[&id, &since, &now]).await? {
                    v.push((r.get(0), r.get(1)));
                }
                v
            }
        };
        Ok(integrate(&points, now))
    }
}

/// The integral of a step function given as `(from, value)` points (sorted by time) up to `end`.
pub fn integrate(points: &[(i64, i64)], end: i64) -> f64 {
    let mut total = 0.0;
    for (i, &(at, v)) in points.iter().enumerate() {
        let until = points.get(i + 1).map_or(end, |p| p.0).min(end);
        if until > at {
            total += (until - at) as f64 * v as f64;
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn cluster(id: &str, name: &str) -> SearchCluster {
        SearchCluster {
            id: id.into(),
            name: name.into(),
            region: "local".into(),
            flavor: "elasticsearch".into(),
            password: "pw".into(),
            idle_timeout_s: 300,
            max_in_flight: 64,
            max_body_mb: 100,
            destructive_requires_name: true,
            parent_id: None,
            parent_at: None,
            created_at: 1,
            password_rotated_at: None,
            indices: 0,
            docs: 0,
            stored_bytes: 0,
        }
    }

    async fn crud(s: &Store) {
        let c = cluster("srch_a", "app");
        assert!(s.insert(&c).await.unwrap());
        assert!(!s.insert(&c).await.unwrap(), "ids are unique");
        assert!(!s.insert(&cluster("srch_b", "app")).await.unwrap(), "names are unique");
        assert_eq!(s.by_name("app").await.unwrap().unwrap().id, "srch_a");
        let mut child = cluster("srch_c", "app-dev");
        child.parent_id = Some("srch_a".into());
        child.parent_at = Some(5);
        child.created_at = 2;
        assert!(s.insert(&child).await.unwrap());
        assert_eq!(s.children("srch_a").await.unwrap().len(), 1);
        let mut c = s.get("srch_a").await.unwrap().unwrap();
        c.flavor = "opensearch".into();
        c.password = "new".into();
        c.max_in_flight = 8;
        s.update(&c).await.unwrap();
        let back = s.get("srch_a").await.unwrap().unwrap();
        assert_eq!((back.flavor.as_str(), back.password.as_str(), back.max_in_flight), ("opensearch", "new", 8));
        assert_eq!(s.list().await.unwrap().iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["app", "app-dev"]);
        assert!(s.always_on().await.unwrap().is_empty());
        c.idle_timeout_s = 0;
        s.update(&c).await.unwrap();
        assert_eq!(s.always_on().await.unwrap().iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["app"]);
        s.set_deleting("srch_a", true).await.unwrap();
        assert!(s.always_on().await.unwrap().is_empty(), "a cluster being deleted is never loaded");
        s.set_deleting("srch_a", false).await.unwrap();
        s.delete("srch_a").await.unwrap();
        assert!(s.get("srch_a").await.unwrap().is_none());
        s.delete("srch_c").await.unwrap();
    }

    async fn changes(s: &Store) {
        let start = s.current_seq().await.unwrap();
        assert_eq!(s.changes_since(start).await.unwrap(), (vec![], start));
        assert!(s.insert(&cluster("srch_x", "x")).await.unwrap());
        assert!(s.insert(&cluster("srch_y", "y")).await.unwrap());
        let (names, after_insert) = s.changes_since(start).await.unwrap();
        assert_eq!(names, ["x", "y"]);
        assert!(after_insert > start);
        // Metering figures aren't changes.
        s.set_stats("srch_x", 10, 1, 2, 3).await.unwrap();
        assert_eq!(s.changes_since(after_insert).await.unwrap().0, Vec::<String>::new());
        let mut x = s.get("srch_x").await.unwrap().unwrap();
        x.password = "rotated".into();
        s.update(&x).await.unwrap();
        let (names, after_update) = s.changes_since(after_insert).await.unwrap();
        assert_eq!(names, ["x"]);
        // Being deleted: hidden from lookups, visible to followers.
        s.set_deleting("srch_y", true).await.unwrap();
        assert!(s.by_name("y").await.unwrap().is_none());
        assert!(s.get("srch_y").await.unwrap().is_none());
        assert_eq!(s.list().await.unwrap().iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["x"]);
        assert!(!s.insert(&cluster("srch_z", "y")).await.unwrap(), "a name being deleted is still taken");
        let (names, after_deleting) = s.changes_since(after_update).await.unwrap();
        assert_eq!(names, ["y"]);
        s.set_deleting("srch_y", false).await.unwrap();
        assert!(s.by_name("y").await.unwrap().is_some());
        // A delete leaves a tombstone.
        s.delete("srch_y").await.unwrap();
        let (names, after_delete) = s.changes_since(after_deleting).await.unwrap();
        assert_eq!(names, ["y"]);
        assert_eq!(s.current_seq().await.unwrap(), after_delete);
        s.delete("srch_x").await.unwrap();
    }

    async fn usage_totals(s: &Store) {
        assert!(s.insert(&cluster("srch_u", "usage")).await.unwrap());
        s.record_usage("srch_u", &UsageSample { at: 100, seconds: 60, loaded_bytes: 1 << 30, requests: 10 }).await.unwrap();
        s.record_usage("srch_u", &UsageSample { at: 160, seconds: 30, loaded_bytes: 2 << 30, requests: 5 }).await.unwrap();
        let t = s.usage_since("srch_u", 0, 200).await.unwrap();
        assert_eq!((t.loaded_seconds, t.requests, t.peak_loaded_bytes), (90, 15, 2 << 30));
        assert!((t.loaded_gib_hours - (60.0 + 60.0) / 3600.0).abs() < 1e-12);
        assert_eq!(s.usage_since("srch_u", 150, 200).await.unwrap().requests, 5);
        s.delete("srch_u").await.unwrap();
        assert_eq!(s.usage_since("srch_u", 0, 200).await.unwrap().requests, 0);
    }

    async fn storage_is_a_step_function(s: &Store) {
        assert!(s.insert(&cluster("srch_s", "storage")).await.unwrap());
        s.set_stats("srch_s", 100, 1, 10, 1000).await.unwrap();
        // An unchanged size logs nothing.
        s.set_stats("srch_s", 150, 1, 12, 1000).await.unwrap();
        s.set_stats("srch_s", 200, 2, 20, 3000).await.unwrap();
        assert_eq!(s.get("srch_s").await.unwrap().unwrap().docs, 20);
        // 100..200 at 1000, 200..300 at 3000.
        assert_eq!(s.storage_byte_seconds("srch_s", 0, 300).await.unwrap(), 100.0 * 1000.0 + 100.0 * 3000.0);
        // A window starting mid-step uses the size in effect then.
        assert_eq!(s.storage_byte_seconds("srch_s", 150, 300).await.unwrap(), 50.0 * 1000.0 + 100.0 * 3000.0);
        assert_eq!(s.storage_byte_seconds("srch_s", 250, 300).await.unwrap(), 50.0 * 3000.0);
        let months = s.usage_since("srch_s", 0, 300).await.unwrap().stored_gib_months;
        assert!((months - 400_000.0 / GIB / MONTH_SECONDS).abs() < 1e-18);
        s.delete("srch_s").await.unwrap();
    }

    async fn all(s: &Store) {
        crud(s).await;
        changes(s).await;
        usage_totals(s).await;
        storage_is_a_step_function(s).await;
    }

    #[tokio::test]
    async fn sqlite() {
        let s = Store::memory().unwrap();
        assert!(!s.is_shared());
        all(&s).await;
    }

    /// Nodes sharing one file start together: every one opens it, the schema is created once.
    #[test]
    fn sqlite_opens_concurrently() {
        let dir = std::env::temp_dir().join(format!("roll-search-store-race-{}-{}", std::process::id(), now_s()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fleet.db");
        let opened: Vec<_> = (0..8)
            .map(|_| {
                let p = path.clone();
                std::thread::spawn(move || Store::open(&p).map(|_| ()))
            })
            .collect();
        for t in opened {
            t.join().unwrap().unwrap();
        }
        let c = Connection::open(&path).unwrap();
        let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v as usize, MIGRATIONS.len());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn sqlite_migrates_an_old_catalog() {
        let dir = std::env::temp_dir().join(format!("search-control-store-{}-{}", std::process::id(), crate::meter::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("search.db");
        {
            let mut conn = Connection::open(&path).unwrap();
            let tx = conn.transaction().unwrap();
            tx.execute_batch(MIGRATIONS[0]).unwrap();
            tx.execute_batch("PRAGMA user_version = 1").unwrap();
            tx.commit().unwrap();
            conn.execute(&format!("INSERT INTO search_clusters ({COLS}) VALUES ('srch_o', 'old', 'local', 'elasticsearch', 'pw', 300, 64, 100, 1, NULL, NULL, 1, NULL, 0, 0, 0)"), []).unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.by_name("old").await.unwrap().unwrap().id, "srch_o");
        s.delete("srch_o").await.unwrap();
        all(&s).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Runs when `ROLL_SEARCH_TEST_PG` names a Postgres URL.
    #[tokio::test]
    async fn postgres() {
        let Ok(url) = std::env::var("ROLL_SEARCH_TEST_PG") else { return };
        let pg = Pg::connect(&url, None, None).await.unwrap();
        // Nodes create the schema at once.
        let (a, b) = tokio::join!(Store::postgres(pg.clone()), Store::postgres(Pg::connect(&url, None, None).await.unwrap()));
        let s = a.unwrap();
        drop(b.unwrap());
        assert!(s.is_shared());
        {
            let c = pg.client().await.unwrap();
            for t in ["roll_search_clusters", "roll_search_usage", "roll_search_storage", "roll_search_tombstones"] {
                c.execute(&format!("DELETE FROM {t}"), &[]).await.unwrap();
            }
        }
        all(&s).await;
    }

    #[test]
    fn integration_edges() {
        assert_eq!(integrate(&[], 10), 0.0);
        assert_eq!(integrate(&[(0, 5)], 10), 50.0);
        assert_eq!(integrate(&[(0, 5), (4, 0)], 10), 20.0);
        // Points past the end don't count.
        assert_eq!(integrate(&[(0, 1), (20, 100)], 10), 10.0);
        assert_eq!(integrate(&[(5, 1)], 3), 0.0);
    }
}

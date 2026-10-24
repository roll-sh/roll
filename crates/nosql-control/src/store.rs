//! Control-plane state for roll nosql in SQLite (its own file, `<home>/nosql/nosql.db`):
//! databases (branches are databases with a parent), imports and usage samples.

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;
use serde_json::{Map, Value};
use std::path::Path;
use std::sync::Mutex;

/// A database (or branch) as the control plane knows it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct NosqlDb {
    /// `n` + 11 lowercase base32 characters (a DNS label).
    pub id: String,
    pub name: String,
    /// `scylla` or `cassandra`.
    pub dialect: String,
    pub datacenter: String,
    pub rack: String,
    pub cluster_name: String,
    /// `cassandra.yaml` / `scylla.yaml` overrides (see `settings.rs`).
    pub settings: Map<String, Value>,
    /// `async` or `sync`.
    pub durability: String,
    /// Seconds without clients before scaling to zero; 0 = always on.
    #[serde(rename = "idle_timeout")]
    pub idle_timeout_s: i64,
    /// Point-in-time restore window in seconds.
    #[serde(rename = "history_retention")]
    pub history_retention_s: i64,
    pub read_only: bool,
    /// `"ks.table.column": "mask_inner(1, 1)"`.
    pub masking: Map<String, Value>,
    /// Unix seconds; the expiry loop deletes the database then.
    pub expires_at: Option<i64>,
    pub parent_id: Option<String>,
    /// Unix seconds of the parent's state the branch (or its last reset) started from.
    pub parent_at: Option<i64>,
    /// Created (and reset) with the parent's schema and roles but no rows.
    pub schema_only: bool,
    /// The platform-managed initial superuser.
    #[serde(skip)]
    pub superuser: String,
    #[serde(skip)]
    pub password: String,
    pub created_at: i64,
}

/// One import into a database (`cql://` live import; `s3://` is accepted but not yet run).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Import {
    pub id: String,
    pub db_id: String,
    /// The source URL with the password (never shown; see [`Import::from_redacted`]).
    #[serde(skip)]
    pub source: String,
    /// Empty: every non-system keyspace.
    pub keyspaces: Vec<String>,
    /// Refresh interval in seconds (`None`: once).
    pub every: Option<i64>,
    /// `queued`, `running`, `done` or `failed`.
    pub state: String,
    pub rows: i64,
    pub tables_done: i64,
    pub tables_total: i64,
    pub error: Option<String>,
    /// Completed runs (each refresh is a new manifest version).
    pub runs: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub next_run_at: Option<i64>,
    pub created_at: i64,
}

impl Import {
    /// The source URL with its password replaced by `***`.
    pub fn from_redacted(&self) -> String {
        redact(&self.source)
    }
}

/// `scheme://user:pass@host` → `scheme://user:***@host`.
pub fn redact(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else { return url.to_string() };
    let (auth, host) = match rest.rsplit_once('@') {
        Some(x) => x,
        None => return url.to_string(),
    };
    match auth.split_once(':') {
        Some((user, _)) => format!("{scheme}://{user}:***@{host}"),
        None => url.to_string(),
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct UsageTotals {
    /// Seconds the database was loaded.
    pub active_seconds: i64,
    pub reads: i64,
    pub writes: i64,
    /// CQL and HTTP requests of every kind (Alternator's included).
    pub requests: i64,
    /// Alternator (DynamoDB API) requests.
    pub alternator_requests: i64,
    /// Sum over samples of connected clients × sample length.
    pub connection_seconds: i64,
    pub net_in_bytes: i64,
    pub net_out_bytes: i64,
}

/// One metering interval of a database.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageSample {
    pub at: i64,
    /// Seconds loaded in this interval (0 for an unloaded database whose counters moved).
    pub seconds: i64,
    pub reads: i64,
    pub writes: i64,
    pub requests: i64,
    pub alternator_requests: i64,
    pub connection_seconds: i64,
    pub net_in: i64,
    pub net_out: i64,
}

pub struct Store {
    conn: Mutex<Connection>,
}

const MIGRATIONS: &[&str] = &[r#"
CREATE TABLE nosql_databases (
  id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, dialect TEXT NOT NULL,
  datacenter TEXT NOT NULL, rack TEXT NOT NULL, cluster_name TEXT NOT NULL,
  settings TEXT NOT NULL, durability TEXT NOT NULL, idle_timeout_s INTEGER NOT NULL,
  history_retention_s INTEGER NOT NULL, read_only INTEGER NOT NULL, masking TEXT NOT NULL,
  expires_at INTEGER, parent_id TEXT, parent_at INTEGER, schema_only INTEGER NOT NULL,
  superuser TEXT NOT NULL, password TEXT NOT NULL, created_at INTEGER NOT NULL
);
CREATE INDEX nosql_databases_expiry ON nosql_databases (expires_at) WHERE expires_at IS NOT NULL;
CREATE TABLE nosql_imports (
  id TEXT PRIMARY KEY, db_id TEXT NOT NULL REFERENCES nosql_databases(id) ON DELETE CASCADE,
  source TEXT NOT NULL, keyspaces TEXT NOT NULL, every_s INTEGER, state TEXT NOT NULL,
  rows INTEGER NOT NULL, tables_done INTEGER NOT NULL, tables_total INTEGER NOT NULL,
  error TEXT, runs INTEGER NOT NULL, started_at INTEGER, finished_at INTEGER,
  next_run_at INTEGER, created_at INTEGER NOT NULL
);
CREATE INDEX nosql_imports_db ON nosql_imports (db_id, created_at);
CREATE TABLE nosql_usage (
  db_id TEXT NOT NULL, at INTEGER NOT NULL, seconds INTEGER NOT NULL, reads INTEGER NOT NULL,
  writes INTEGER NOT NULL, requests INTEGER NOT NULL, connection_seconds INTEGER NOT NULL,
  net_in INTEGER NOT NULL, net_out INTEGER NOT NULL
);
CREATE INDEX nosql_usage_db ON nosql_usage (db_id, at);
"#, r#"
ALTER TABLE nosql_usage ADD COLUMN alternator_requests INTEGER NOT NULL DEFAULT 0;
"#];

const DB_COLS: &str = "id, name, dialect, datacenter, rack, cluster_name, settings, durability, idle_timeout_s, history_retention_s, read_only, masking, expires_at, parent_id, parent_at, schema_only, superuser, password, created_at";
const IMPORT_COLS: &str = "id, db_id, source, keyspaces, every_s, state, rows, tables_done, tables_total, error, runs, started_at, finished_at, next_run_at, created_at";

fn json_map(r: &Row, i: usize) -> rusqlite::Result<Map<String, Value>> {
    let s: String = r.get(i)?;
    Ok(serde_json::from_str(&s).unwrap_or_default())
}

fn db_row(r: &Row) -> rusqlite::Result<NosqlDb> {
    Ok(NosqlDb {
        id: r.get(0)?,
        name: r.get(1)?,
        dialect: r.get(2)?,
        datacenter: r.get(3)?,
        rack: r.get(4)?,
        cluster_name: r.get(5)?,
        settings: json_map(r, 6)?,
        durability: r.get(7)?,
        idle_timeout_s: r.get(8)?,
        history_retention_s: r.get(9)?,
        read_only: r.get(10)?,
        masking: json_map(r, 11)?,
        expires_at: r.get(12)?,
        parent_id: r.get(13)?,
        parent_at: r.get(14)?,
        schema_only: r.get(15)?,
        superuser: r.get(16)?,
        password: r.get(17)?,
        created_at: r.get(18)?,
    })
}

fn import_row(r: &Row) -> rusqlite::Result<Import> {
    let ks: String = r.get(3)?;
    Ok(Import {
        id: r.get(0)?,
        db_id: r.get(1)?,
        source: r.get(2)?,
        keyspaces: serde_json::from_str(&ks).unwrap_or_default(),
        every: r.get(4)?,
        state: r.get(5)?,
        rows: r.get(6)?,
        tables_done: r.get(7)?,
        tables_total: r.get(8)?,
        error: r.get(9)?,
        runs: r.get(10)?,
        started_at: r.get(11)?,
        finished_at: r.get(12)?,
        next_run_at: r.get(13)?,
        created_at: r.get(14)?,
    })
}

fn to_json(m: &Map<String, Value>) -> String {
    serde_json::to_string(m).unwrap_or_else(|_| "{}".into())
}

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let mut conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;")?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            let tx = conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.execute_batch(&format!("PRAGMA user_version = {}", i + 1))?;
            tx.commit()?;
        }
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn memory() -> anyhow::Result<Self> {
        Self::open(Path::new(":memory:"))
    }

    /// The schema version (number of migrations applied).
    pub fn version(&self) -> anyhow::Result<i64> {
        self.with(|c| c.query_row("PRAGMA user_version", [], |r| r.get(0)))
    }

    fn with<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> anyhow::Result<T> {
        Ok(f(&self.conn.lock().unwrap_or_else(|e| e.into_inner()))?)
    }

    pub fn insert_db(&self, d: &NosqlDb) -> anyhow::Result<()> {
        self.with(|c| {
            c.execute(
                &format!("INSERT INTO nosql_databases ({DB_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)"),
                params![
                    d.id,
                    d.name,
                    d.dialect,
                    d.datacenter,
                    d.rack,
                    d.cluster_name,
                    to_json(&d.settings),
                    d.durability,
                    d.idle_timeout_s,
                    d.history_retention_s,
                    d.read_only,
                    to_json(&d.masking),
                    d.expires_at,
                    d.parent_id,
                    d.parent_at,
                    d.schema_only,
                    d.superuser,
                    d.password,
                    d.created_at
                ],
            )
            .map(|_| ())
        })
    }

    /// Rewrites every column but `id` and `created_at`.
    pub fn update_db(&self, d: &NosqlDb) -> anyhow::Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE nosql_databases SET name = ?2, dialect = ?3, datacenter = ?4, rack = ?5, cluster_name = ?6, settings = ?7,
                 durability = ?8, idle_timeout_s = ?9, history_retention_s = ?10, read_only = ?11, masking = ?12, expires_at = ?13,
                 parent_id = ?14, parent_at = ?15, schema_only = ?16, superuser = ?17, password = ?18 WHERE id = ?1",
                params![
                    d.id,
                    d.name,
                    d.dialect,
                    d.datacenter,
                    d.rack,
                    d.cluster_name,
                    to_json(&d.settings),
                    d.durability,
                    d.idle_timeout_s,
                    d.history_retention_s,
                    d.read_only,
                    to_json(&d.masking),
                    d.expires_at,
                    d.parent_id,
                    d.parent_at,
                    d.schema_only,
                    d.superuser,
                    d.password
                ],
            )
            .map(|_| ())
        })
    }

    pub fn db(&self, id: &str) -> anyhow::Result<Option<NosqlDb>> {
        self.with(|c| c.query_row(&format!("SELECT {DB_COLS} FROM nosql_databases WHERE id = ?1"), [id], db_row).optional())
    }

    pub fn db_by_name(&self, name: &str) -> anyhow::Result<Option<NosqlDb>> {
        self.with(|c| c.query_row(&format!("SELECT {DB_COLS} FROM nosql_databases WHERE name = ?1"), [name], db_row).optional())
    }

    pub fn dbs(&self) -> anyhow::Result<Vec<NosqlDb>> {
        self.with(|c| {
            let mut st = c.prepare(&format!("SELECT {DB_COLS} FROM nosql_databases ORDER BY created_at, id"))?;
            st.query_map([], db_row)?.collect()
        })
    }

    /// Databases with `expires_at <= now`.
    pub fn expired(&self, now: i64) -> anyhow::Result<Vec<NosqlDb>> {
        self.with(|c| {
            let mut st = c.prepare(&format!("SELECT {DB_COLS} FROM nosql_databases WHERE expires_at IS NOT NULL AND expires_at <= ?1 ORDER BY expires_at, id"))?;
            st.query_map([now], db_row)?.collect()
        })
    }

    /// Direct branches of a database.
    pub fn children(&self, id: &str) -> anyhow::Result<Vec<NosqlDb>> {
        self.with(|c| {
            let mut st = c.prepare(&format!("SELECT {DB_COLS} FROM nosql_databases WHERE parent_id = ?1 ORDER BY created_at, id"))?;
            st.query_map([id], db_row)?.collect()
        })
    }

    pub fn delete_db(&self, id: &str) -> anyhow::Result<()> {
        self.with(|c| {
            c.execute("DELETE FROM nosql_databases WHERE id = ?1", [id])?;
            c.execute("DELETE FROM nosql_usage WHERE db_id = ?1", [id]).map(|_| ())
        })
    }

    pub fn insert_import(&self, i: &Import) -> anyhow::Result<()> {
        self.with(|c| {
            c.execute(
                &format!("INSERT INTO nosql_imports ({IMPORT_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)"),
                params![
                    i.id,
                    i.db_id,
                    i.source,
                    serde_json::to_string(&i.keyspaces).unwrap_or_else(|_| "[]".into()),
                    i.every,
                    i.state,
                    i.rows,
                    i.tables_done,
                    i.tables_total,
                    i.error,
                    i.runs,
                    i.started_at,
                    i.finished_at,
                    i.next_run_at,
                    i.created_at
                ],
            )
            .map(|_| ())
        })
    }

    /// Progress and state (everything that changes while an import runs).
    pub fn update_import(&self, i: &Import) -> anyhow::Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE nosql_imports SET state = ?2, rows = ?3, tables_done = ?4, tables_total = ?5, error = ?6, runs = ?7,
                 started_at = ?8, finished_at = ?9, next_run_at = ?10 WHERE id = ?1",
                params![i.id, i.state, i.rows, i.tables_done, i.tables_total, i.error, i.runs, i.started_at, i.finished_at, i.next_run_at],
            )
            .map(|_| ())
        })
    }

    pub fn import(&self, id: &str) -> anyhow::Result<Option<Import>> {
        self.with(|c| c.query_row(&format!("SELECT {IMPORT_COLS} FROM nosql_imports WHERE id = ?1"), [id], import_row).optional())
    }

    pub fn imports(&self, db_id: &str) -> anyhow::Result<Vec<Import>> {
        self.with(|c| {
            let mut st = c.prepare(&format!("SELECT {IMPORT_COLS} FROM nosql_imports WHERE db_id = ?1 ORDER BY created_at, id"))?;
            st.query_map([db_id], import_row)?.collect()
        })
    }

    /// Scheduled imports whose next run is due.
    pub fn due_imports(&self, now: i64) -> anyhow::Result<Vec<Import>> {
        self.with(|c| {
            let mut st = c.prepare(&format!(
                "SELECT {IMPORT_COLS} FROM nosql_imports WHERE next_run_at IS NOT NULL AND next_run_at <= ?1 AND state != 'running' ORDER BY next_run_at, id"
            ))?;
            st.query_map([now], import_row)?.collect()
        })
    }

    /// Imports a restart interrupted: marked failed (scheduled ones run again when due).
    pub fn fail_running_imports(&self, now: i64) -> anyhow::Result<usize> {
        self.with(|c| {
            c.execute(
                "UPDATE nosql_imports SET state = 'failed', error = 'interrupted by a restart', finished_at = ?1 WHERE state IN ('running', 'queued')",
                [now],
            )
        })
    }

    pub fn record_usage(&self, db_id: &str, u: &UsageSample) -> anyhow::Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO nosql_usage (db_id, at, seconds, reads, writes, requests, connection_seconds, net_in, net_out, alternator_requests) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![db_id, u.at, u.seconds, u.reads, u.writes, u.requests, u.connection_seconds, u.net_in, u.net_out, u.alternator_requests],
            )
            .map(|_| ())
        })
    }

    pub fn usage_since(&self, db_id: &str, since: i64) -> anyhow::Result<UsageTotals> {
        self.with(|c| {
            c.query_row(
                "SELECT COALESCE(SUM(seconds), 0), COALESCE(SUM(reads), 0), COALESCE(SUM(writes), 0), COALESCE(SUM(requests), 0),
                        COALESCE(SUM(connection_seconds), 0), COALESCE(SUM(net_in), 0), COALESCE(SUM(net_out), 0),
                        COALESCE(SUM(alternator_requests), 0)
                 FROM nosql_usage WHERE db_id = ?1 AND at >= ?2",
                params![db_id, since],
                |r| {
                    Ok(UsageTotals {
                        active_seconds: r.get(0)?,
                        reads: r.get(1)?,
                        writes: r.get(2)?,
                        requests: r.get(3)?,
                        connection_seconds: r.get(4)?,
                        net_in_bytes: r.get(5)?,
                        net_out_bytes: r.get(6)?,
                        alternator_requests: r.get(7)?,
                    })
                },
            )
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) fn sample_db(id: &str, name: &str) -> NosqlDb {
        NosqlDb {
            id: id.into(),
            name: name.into(),
            dialect: "scylla".into(),
            datacenter: "us-east-1".into(),
            rack: "rack1".into(),
            cluster_name: name.into(),
            settings: json!({"tombstone_warn_threshold": 10}).as_object().unwrap().clone(),
            durability: "async".into(),
            idle_timeout_s: 300,
            history_retention_s: 86_400,
            read_only: false,
            masking: Map::new(),
            expires_at: None,
            parent_id: None,
            parent_at: None,
            schema_only: false,
            superuser: "cassandra".into(),
            password: "pw".into(),
            created_at: 1,
        }
    }

    #[test]
    fn migrations_are_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nosql.db");
        let s = Store::open(&path).unwrap();
        assert_eq!(s.version().unwrap(), MIGRATIONS.len() as i64);
        s.insert_db(&sample_db("na", "a")).unwrap();
        drop(s);
        // Reopening applies nothing again and keeps the data.
        let s = Store::open(&path).unwrap();
        assert_eq!(s.version().unwrap(), MIGRATIONS.len() as i64);
        assert_eq!(s.db("na").unwrap().unwrap().name, "a");
    }

    #[test]
    fn databases() {
        let s = Store::memory().unwrap();
        let mut d = sample_db("na", "app");
        s.insert_db(&d).unwrap();
        assert!(s.insert_db(&d).is_err(), "duplicate id");
        assert!(s.insert_db(&sample_db("nb", "app")).is_err(), "duplicate name");
        assert_eq!(s.db("na").unwrap().unwrap(), d);
        assert_eq!(s.db_by_name("app").unwrap().unwrap().id, "na");
        assert!(s.db("nx").unwrap().is_none());
        d.read_only = true;
        d.expires_at = Some(50);
        d.masking = json!({"ks.t.c": "mask_null()"}).as_object().unwrap().clone();
        d.password = "new".into();
        s.update_db(&d).unwrap();
        assert_eq!(s.db("na").unwrap().unwrap(), d);
        let mut b = sample_db("nb", "app-pr");
        b.parent_id = Some("na".into());
        b.created_at = 2;
        s.insert_db(&b).unwrap();
        assert_eq!(s.children("na").unwrap().len(), 1);
        assert_eq!(s.dbs().unwrap().iter().map(|d| d.id.as_str()).collect::<Vec<_>>(), ["na", "nb"]);
        assert_eq!(s.expired(49).unwrap().len(), 0);
        assert_eq!(s.expired(50).unwrap()[0].id, "na");
        s.delete_db("na").unwrap();
        assert!(s.db("na").unwrap().is_none());
    }

    #[test]
    fn imports_and_usage() {
        let s = Store::memory().unwrap();
        s.insert_db(&sample_db("na", "app")).unwrap();
        let mut i = Import {
            id: "i1".into(),
            db_id: "na".into(),
            source: "cql://u:secret@h:9042".into(),
            keyspaces: vec!["app".into()],
            every: Some(3600),
            state: "queued".into(),
            rows: 0,
            tables_done: 0,
            tables_total: 0,
            error: None,
            runs: 0,
            started_at: None,
            finished_at: None,
            next_run_at: None,
            created_at: 5,
        };
        s.insert_import(&i).unwrap();
        assert_eq!(s.import("i1").unwrap().unwrap(), i);
        assert_eq!(i.from_redacted(), "cql://u:***@h:9042");
        i.state = "running".into();
        i.rows = 42;
        i.next_run_at = Some(100);
        s.update_import(&i).unwrap();
        assert_eq!(s.imports("na").unwrap(), vec![i.clone()]);
        assert!(s.due_imports(200).unwrap().is_empty(), "running imports aren't due");
        assert_eq!(s.fail_running_imports(7).unwrap(), 1);
        let failed = s.import("i1").unwrap().unwrap();
        assert_eq!((failed.state.as_str(), failed.error.as_deref()), ("failed", Some("interrupted by a restart")));
        assert_eq!(s.due_imports(99).unwrap().len(), 0);
        assert_eq!(s.due_imports(100).unwrap().len(), 1);

        let u = UsageSample { at: 10, seconds: 60, reads: 5, writes: 7, requests: 20, alternator_requests: 3, connection_seconds: 120, net_in: 100, net_out: 200 };
        s.record_usage("na", &u).unwrap();
        s.record_usage("na", &UsageSample { at: 70, ..u.clone() }).unwrap();
        let t = s.usage_since("na", 0).unwrap();
        assert_eq!(t, UsageTotals { active_seconds: 120, reads: 10, writes: 14, requests: 40, alternator_requests: 6, connection_seconds: 240, net_in_bytes: 200, net_out_bytes: 400 });
        assert_eq!(s.usage_since("na", 11).unwrap().reads, 5);
        // Deleting a database removes its imports and usage.
        s.delete_db("na").unwrap();
        assert!(s.imports("na").unwrap().is_empty());
        assert_eq!(s.usage_since("na", 0).unwrap(), UsageTotals::default());
    }

    #[test]
    fn redaction() {
        assert_eq!(redact("cql://a:b@h:1"), "cql://a:***@h:1");
        assert_eq!(redact("cql://a:b:c@d@h:1"), "cql://a:***@h:1");
        assert_eq!(redact("cql://h:1"), "cql://h:1");
        assert_eq!(redact("s3://bucket/path"), "s3://bucket/path");
        assert_eq!(redact("nonsense"), "nonsense");
    }
}

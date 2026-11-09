//! Control-plane state for Redis in SQLite (its own file, `redis.db`).

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;
use std::path::Path;
use std::sync::Mutex;

#[derive(Clone, Debug, Serialize)]
pub struct RedisDb {
    pub id: String,
    pub name: String,
    pub region: String,
    #[serde(skip)]
    pub password: String,
    /// `maxmemory`; writes beyond it evict (per `eviction_policy`) or fail with OOM.
    pub max_memory_mb: i64,
    pub eviction_policy: String,
    /// `async`: acknowledged writes reach object storage within a second (like `appendfsync
    /// everysec`). `sync`: replies wait until the write is in object storage.
    pub durability: String,
    /// Seconds without clients before scaling to zero; 0 = always on.
    pub idle_timeout_s: i64,
    /// Point-in-time restore window.
    pub history_retention_s: i64,
    pub parent_id: Option<String>,
    /// Unix seconds of the parent's state the branch started from.
    pub parent_at: Option<i64>,
    pub created_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Replica {
    pub id: String,
    pub db_id: String,
    pub region: String,
    #[serde(skip)]
    pub password: String,
    pub created_at: i64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct UsageTotals {
    /// Seconds the database was loaded (in memory).
    pub active_seconds: i64,
    /// Integral of memory over loaded time.
    pub gib_hours: f64,
    pub commands: i64,
    pub net_in_bytes: i64,
    pub net_out_bytes: i64,
    pub peak_memory_bytes: i64,
}

/// One metering interval of a loaded database.
#[derive(Clone, Debug, Default)]
pub struct UsageSample {
    pub at: i64,
    pub seconds: i64,
    pub mem_bytes: i64,
    pub commands: i64,
    pub net_in: i64,
    pub net_out: i64,
}

pub struct Store {
    conn: Mutex<Connection>,
}

const MIGRATIONS: &[&str] = &[r#"
CREATE TABLE redis_databases (
  id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, region TEXT NOT NULL, password TEXT NOT NULL,
  max_memory_mb INTEGER NOT NULL, eviction_policy TEXT NOT NULL, durability TEXT NOT NULL,
  idle_timeout_s INTEGER NOT NULL, history_retention_s INTEGER NOT NULL,
  parent_id TEXT, parent_at INTEGER, created_at INTEGER NOT NULL
);
CREATE TABLE redis_replicas (
  id TEXT PRIMARY KEY, db_id TEXT NOT NULL REFERENCES redis_databases(id) ON DELETE CASCADE,
  region TEXT NOT NULL, password TEXT NOT NULL, created_at INTEGER NOT NULL
);
CREATE TABLE redis_usage (
  db_id TEXT NOT NULL, at INTEGER NOT NULL, seconds INTEGER NOT NULL, mem_bytes INTEGER NOT NULL,
  commands INTEGER NOT NULL, net_in INTEGER NOT NULL, net_out INTEGER NOT NULL
);
CREATE INDEX redis_usage_db ON redis_usage (db_id, at);
"#];

const DB_COLS: &str = "id, name, region, password, max_memory_mb, eviction_policy, durability, idle_timeout_s, history_retention_s, parent_id, parent_at, created_at";

fn db_row(r: &Row) -> rusqlite::Result<RedisDb> {
    Ok(RedisDb {
        id: r.get(0)?,
        name: r.get(1)?,
        region: r.get(2)?,
        password: r.get(3)?,
        max_memory_mb: r.get(4)?,
        eviction_policy: r.get(5)?,
        durability: r.get(6)?,
        idle_timeout_s: r.get(7)?,
        history_retention_s: r.get(8)?,
        parent_id: r.get(9)?,
        parent_at: r.get(10)?,
        created_at: r.get(11)?,
    })
}

fn replica_row(r: &Row) -> rusqlite::Result<Replica> {
    Ok(Replica { id: r.get(0)?, db_id: r.get(1)?, region: r.get(2)?, password: r.get(3)?, created_at: r.get(4)? })
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

    fn with<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> anyhow::Result<T> {
        Ok(f(&self.conn.lock().unwrap())?)
    }

    pub fn insert_db(&self, d: &RedisDb) -> anyhow::Result<()> {
        self.with(|c| {
            c.execute(
                &format!("INSERT INTO redis_databases ({DB_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"),
                params![
                    d.id,
                    d.name,
                    d.region,
                    d.password,
                    d.max_memory_mb,
                    d.eviction_policy,
                    d.durability,
                    d.idle_timeout_s,
                    d.history_retention_s,
                    d.parent_id,
                    d.parent_at,
                    d.created_at
                ],
            )
            .map(|_| ())
        })
    }

    pub fn update_db(&self, d: &RedisDb) -> anyhow::Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE redis_databases SET name = ?2, password = ?3, max_memory_mb = ?4, eviction_policy = ?5, durability = ?6,
                 idle_timeout_s = ?7, history_retention_s = ?8 WHERE id = ?1",
                params![d.id, d.name, d.password, d.max_memory_mb, d.eviction_policy, d.durability, d.idle_timeout_s, d.history_retention_s],
            )
            .map(|_| ())
        })
    }

    pub fn db(&self, id: &str) -> anyhow::Result<Option<RedisDb>> {
        self.with(|c| c.query_row(&format!("SELECT {DB_COLS} FROM redis_databases WHERE id = ?1"), [id], db_row).optional())
    }

    pub fn db_by_name(&self, name: &str) -> anyhow::Result<Option<RedisDb>> {
        self.with(|c| c.query_row(&format!("SELECT {DB_COLS} FROM redis_databases WHERE name = ?1"), [name], db_row).optional())
    }

    pub fn dbs(&self) -> anyhow::Result<Vec<RedisDb>> {
        self.with(|c| {
            let mut st = c.prepare(&format!("SELECT {DB_COLS} FROM redis_databases ORDER BY created_at, id"))?;
            st.query_map([], db_row)?.collect()
        })
    }

    pub fn delete_db(&self, id: &str) -> anyhow::Result<()> {
        self.with(|c| {
            c.execute("DELETE FROM redis_databases WHERE id = ?1", [id])?;
            c.execute("DELETE FROM redis_usage WHERE db_id = ?1", [id]).map(|_| ())
        })
    }

    pub fn insert_replica(&self, r: &Replica) -> anyhow::Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO redis_replicas (id, db_id, region, password, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![r.id, r.db_id, r.region, r.password, r.created_at],
            )
            .map(|_| ())
        })
    }

    pub fn replica(&self, id: &str) -> anyhow::Result<Option<Replica>> {
        self.with(|c| c.query_row("SELECT id, db_id, region, password, created_at FROM redis_replicas WHERE id = ?1", [id], replica_row).optional())
    }

    pub fn replicas(&self, db_id: &str) -> anyhow::Result<Vec<Replica>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT id, db_id, region, password, created_at FROM redis_replicas WHERE db_id = ?1 ORDER BY created_at, id")?;
            st.query_map([db_id], replica_row)?.collect()
        })
    }

    pub fn delete_replica(&self, id: &str) -> anyhow::Result<()> {
        self.with(|c| c.execute("DELETE FROM redis_replicas WHERE id = ?1", [id]).map(|_| ()))
    }

    pub fn record_usage(&self, db_id: &str, u: &UsageSample) -> anyhow::Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO redis_usage (db_id, at, seconds, mem_bytes, commands, net_in, net_out) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![db_id, u.at, u.seconds, u.mem_bytes, u.commands, u.net_in, u.net_out],
            )
            .map(|_| ())
        })
    }

    pub fn usage_since(&self, db_id: &str, since: i64) -> anyhow::Result<UsageTotals> {
        self.with(|c| {
            c.query_row(
                "SELECT COALESCE(SUM(seconds), 0), COALESCE(SUM(seconds * mem_bytes), 0.0), COALESCE(SUM(commands), 0),
                        COALESCE(SUM(net_in), 0), COALESCE(SUM(net_out), 0), COALESCE(MAX(mem_bytes), 0)
                 FROM redis_usage WHERE db_id = ?1 AND at >= ?2",
                params![db_id, since],
                |r| {
                    let byte_seconds: f64 = r.get(1)?;
                    Ok(UsageTotals {
                        active_seconds: r.get(0)?,
                        gib_hours: byte_seconds / (1u64 << 30) as f64 / 3600.0,
                        commands: r.get(2)?,
                        net_in_bytes: r.get(3)?,
                        net_out_bytes: r.get(4)?,
                        peak_memory_bytes: r.get(5)?,
                    })
                },
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crud_and_usage() {
        let s = Store::memory().unwrap();
        let d = RedisDb {
            id: "rdb_a".into(),
            name: "cache".into(),
            region: "local".into(),
            password: "pw".into(),
            max_memory_mb: 256,
            eviction_policy: "noeviction".into(),
            durability: "async".into(),
            idle_timeout_s: 300,
            history_retention_s: 86_400,
            parent_id: None,
            parent_at: None,
            created_at: 1,
        };
        s.insert_db(&d).unwrap();
        assert!(s.insert_db(&d).is_err());
        assert_eq!(s.db_by_name("cache").unwrap().unwrap().id, "rdb_a");
        s.insert_replica(&Replica { id: "rre_1".into(), db_id: "rdb_a".into(), region: "local".into(), password: "x".into(), created_at: 2 }).unwrap();
        assert_eq!(s.replicas("rdb_a").unwrap().len(), 1);
        let u = UsageSample { at: 10, seconds: 60, mem_bytes: 1 << 30, commands: 1000, net_in: 5, net_out: 7 };
        s.record_usage("rdb_a", &u).unwrap();
        s.record_usage("rdb_a", &UsageSample { at: 70, commands: 500, ..u }).unwrap();
        let u = s.usage_since("rdb_a", 0).unwrap();
        assert_eq!(u.active_seconds, 120);
        assert!((u.gib_hours - 120.0 / 3600.0).abs() < 1e-9);
        assert_eq!(u.commands, 1500);
        s.delete_db("rdb_a").unwrap();
        assert!(s.replicas("rdb_a").unwrap().is_empty());
        assert_eq!(s.usage_since("rdb_a", 0).unwrap().commands, 0);
    }
}

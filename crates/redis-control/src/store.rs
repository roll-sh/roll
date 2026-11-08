//! Control-plane state for Redis: databases, replicas, credentials and metering, through
//! `code-sql`. On one node that is SQLite (`redis.db`, the file it always was); in a cluster it is
//! the shared system database (`ROLL_SYSTEM_DATABASE`, schema `redis`), so every node sees the
//! same catalog and credentials.

use code_sql::{Db, OptionalExtension, Row, params};
use serde::Serialize;

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
    db: Db,
}

/// The tables (SQLite's dialect; translated for Postgres). The same as the SQLite file of
/// earlier releases had, so an existing `redis.db` opens as it is.
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS redis_databases (
  id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, region TEXT NOT NULL, password TEXT NOT NULL,
  max_memory_mb INTEGER NOT NULL, eviction_policy TEXT NOT NULL, durability TEXT NOT NULL,
  idle_timeout_s INTEGER NOT NULL, history_retention_s INTEGER NOT NULL,
  parent_id TEXT, parent_at INTEGER, created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS redis_replicas (
  id TEXT PRIMARY KEY, db_id TEXT NOT NULL,
  region TEXT NOT NULL, password TEXT NOT NULL, created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS redis_replicas_db ON redis_replicas (db_id);
CREATE TABLE IF NOT EXISTS redis_usage (
  db_id TEXT NOT NULL, at INTEGER NOT NULL, seconds INTEGER NOT NULL, mem_bytes INTEGER NOT NULL,
  commands INTEGER NOT NULL, net_in INTEGER NOT NULL, net_out INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS redis_usage_db ON redis_usage (db_id, at);
"#;

const DB_COLS: &str = "id, name, region, password, max_memory_mb, eviction_policy, durability, idle_timeout_s, history_retention_s, parent_id, parent_at, created_at";
const REPLICA_COLS: &str = "id, db_id, region, password, created_at";

fn db_row(r: &Row) -> code_sql::Result<RedisDb> {
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

fn replica_row(r: &Row) -> code_sql::Result<Replica> {
    Ok(Replica { id: r.get(0)?, db_id: r.get(1)?, region: r.get(2)?, password: r.get(3)?, created_at: r.get(4)? })
}

impl Store {
    /// The catalog on `db`, its tables created if they aren't there.
    pub fn open(db: Db) -> anyhow::Result<Self> {
        db.ensure_schema(SCHEMA)?;
        Ok(Self { db })
    }

    /// A catalog in a SQLite file of its own (one node).
    pub fn sqlite(path: &std::path::Path) -> anyhow::Result<Self> {
        Self::open(Db::open_sqlite(path, code_sql::SqliteOptions::default())?)
    }

    pub fn memory() -> anyhow::Result<Self> {
        let dir = std::env::temp_dir().join(format!("roll-redis-store-{}-{}", std::process::id(), redis_engine::util::now_ms()));
        std::fs::create_dir_all(&dir)?;
        Self::sqlite(&dir.join("redis.db"))
    }

    pub fn sql(&self) -> &Db {
        &self.db
    }

    /// Copy what a node's own SQLite catalog (an earlier release's, or a node that ran alone)
    /// holds into this one, keeping what's here: a node joining a cluster brings its databases.
    pub fn import(&self, from: &Store) -> anyhow::Result<usize> {
        let (dbs, replicas) = (from.dbs()?, from.db.read(|c| c.query(&format!("SELECT {REPLICA_COLS} FROM redis_replicas"), (), replica_row))?);
        let n = self.db.write(|c| {
            let mut n = 0;
            for d in &dbs {
                n += c.execute(
                    &format!("INSERT INTO redis_databases ({DB_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) ON CONFLICT DO NOTHING"),
                    params![d.id, d.name, d.region, d.password, d.max_memory_mb, d.eviction_policy, d.durability, d.idle_timeout_s, d.history_retention_s, d.parent_id, d.parent_at, d.created_at],
                )?;
            }
            for r in &replicas {
                c.execute(
                    &format!("INSERT INTO redis_replicas ({REPLICA_COLS}) VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT DO NOTHING"),
                    params![r.id, r.db_id, r.region, r.password, r.created_at],
                )?;
            }
            Ok::<_, code_sql::Error>(n)
        })?;
        Ok(n)
    }

    pub fn insert_db(&self, d: &RedisDb) -> anyhow::Result<()> {
        self.db.execute(
            &format!("INSERT INTO redis_databases ({DB_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"),
            params![d.id, d.name, d.region, d.password, d.max_memory_mb, d.eviction_policy, d.durability, d.idle_timeout_s, d.history_retention_s, d.parent_id, d.parent_at, d.created_at],
        )?;
        Ok(())
    }

    pub fn update_db(&self, d: &RedisDb) -> anyhow::Result<()> {
        self.db.execute(
            "UPDATE redis_databases SET name = ?2, password = ?3, max_memory_mb = ?4, eviction_policy = ?5, durability = ?6,
             idle_timeout_s = ?7, history_retention_s = ?8 WHERE id = ?1",
            params![d.id, d.name, d.password, d.max_memory_mb, d.eviction_policy, d.durability, d.idle_timeout_s, d.history_retention_s],
        )?;
        Ok(())
    }

    pub fn db(&self, id: &str) -> anyhow::Result<Option<RedisDb>> {
        Ok(self.db.query_row(&format!("SELECT {DB_COLS} FROM redis_databases WHERE id = ?1"), [id], db_row).optional()?)
    }

    pub fn db_by_name(&self, name: &str) -> anyhow::Result<Option<RedisDb>> {
        Ok(self.db.query_row(&format!("SELECT {DB_COLS} FROM redis_databases WHERE name = ?1"), [name], db_row).optional()?)
    }

    pub fn dbs(&self) -> anyhow::Result<Vec<RedisDb>> {
        Ok(self.db.query(&format!("SELECT {DB_COLS} FROM redis_databases ORDER BY created_at, id"), (), db_row)?)
    }

    pub fn delete_db(&self, id: &str) -> anyhow::Result<()> {
        self.db.write(|c| {
            c.execute("DELETE FROM redis_replicas WHERE db_id = ?1", [id])?;
            c.execute("DELETE FROM redis_databases WHERE id = ?1", [id])?;
            c.execute("DELETE FROM redis_usage WHERE db_id = ?1", [id])?;
            Ok::<_, code_sql::Error>(())
        })?;
        Ok(())
    }

    pub fn insert_replica(&self, r: &Replica) -> anyhow::Result<()> {
        self.db.execute(&format!("INSERT INTO redis_replicas ({REPLICA_COLS}) VALUES (?1, ?2, ?3, ?4, ?5)"), params![r.id, r.db_id, r.region, r.password, r.created_at])?;
        Ok(())
    }

    pub fn replica(&self, id: &str) -> anyhow::Result<Option<Replica>> {
        Ok(self.db.query_row(&format!("SELECT {REPLICA_COLS} FROM redis_replicas WHERE id = ?1"), [id], replica_row).optional()?)
    }

    pub fn replicas(&self, db_id: &str) -> anyhow::Result<Vec<Replica>> {
        Ok(self.db.query(&format!("SELECT {REPLICA_COLS} FROM redis_replicas WHERE db_id = ?1 ORDER BY created_at, id"), [db_id], replica_row)?)
    }

    pub fn delete_replica(&self, id: &str) -> anyhow::Result<()> {
        self.db.execute("DELETE FROM redis_replicas WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn record_usage(&self, db_id: &str, u: &UsageSample) -> anyhow::Result<()> {
        self.db.execute(
            "INSERT INTO redis_usage (db_id, at, seconds, mem_bytes, commands, net_in, net_out) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![db_id, u.at, u.seconds, u.mem_bytes, u.commands, u.net_in, u.net_out],
        )?;
        Ok(())
    }

    pub fn usage_since(&self, db_id: &str, since: i64) -> anyhow::Result<UsageTotals> {
        Ok(self.db.query_row(
            "SELECT COALESCE(SUM(seconds), 0), COALESCE(SUM(seconds * mem_bytes), 0), COALESCE(SUM(commands), 0),
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
        )?)
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
        // A node's own catalog joins a shared one without overwriting it.
        let shared = Store::memory().unwrap();
        assert_eq!(shared.import(&s).unwrap(), 1);
        assert_eq!(shared.import(&s).unwrap(), 0);
        assert_eq!(shared.replicas("rdb_a").unwrap().len(), 1);
        s.delete_db("rdb_a").unwrap();
        assert!(s.replicas("rdb_a").unwrap().is_empty());
        assert_eq!(s.usage_since("rdb_a", 0).unwrap().commands, 0);
    }
}

//! Control-plane state for roll search in SQLite (its own file, `search.db`).
//!
//! - `search_clusters`: one row per cluster (name, dialect, password, limits, branch origin) plus
//!   the last figures metering saw (indices, documents, stored bytes), so listing clusters never
//!   wakes them or touches object storage.
//! - `search_usage`: one row per metering interval of a *loaded* cluster: loaded seconds, the
//!   estimated resident bytes, requests. An idle cluster adds no rows.
//! - `search_storage`: a change log of each cluster's stored bytes. Storage accrues while a
//!   cluster sleeps too, so instead of a row per cluster per minute the log gets a row only when
//!   the size changes, and GB-months are the integral of that step function over the window.
//!
//! Migrations are append-only and tracked with `PRAGMA user_version`, like the other control
//! planes.

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;
use std::path::Path;
use std::sync::Mutex;

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

pub struct Store {
    conn: Mutex<Connection>,
}

const MIGRATIONS: &[&str] = &[r#"
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
"#];

const COLS: &str = "id, name, region, flavor, password, idle_timeout_s, max_in_flight, max_body_mb, destructive_requires_name, \
                    parent_id, parent_at, created_at, password_rotated_at, indices, docs, stored_bytes";

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

/// Seconds in the 30-day month the GB-month figures use (AWS bills storage per 730-hour month;
/// a 30-day month keeps `days / 30` exact in the dashboard).
pub const MONTH_SECONDS: f64 = 30.0 * 86_400.0;
const GIB: f64 = (1u64 << 30) as f64;

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
        // A panic while holding the lock can't leave SQLite inconsistent (statements are atomic).
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        Ok(f(&conn)?)
    }

    pub fn insert(&self, c: &SearchCluster) -> anyhow::Result<()> {
        self.with(|x| {
            x.execute(
                &format!("INSERT INTO search_clusters ({COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)"),
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
        })
    }

    /// Saves the settings and the password (not the metering figures: see [`Store::set_stats`]).
    pub fn update(&self, c: &SearchCluster) -> anyhow::Result<()> {
        self.with(|x| {
            x.execute(
                "UPDATE search_clusters SET flavor = ?2, password = ?3, idle_timeout_s = ?4, max_in_flight = ?5, max_body_mb = ?6,
                 destructive_requires_name = ?7, password_rotated_at = ?8 WHERE id = ?1",
                params![c.id, c.flavor, c.password, c.idle_timeout_s, c.max_in_flight, c.max_body_mb, c.destructive_requires_name, c.password_rotated_at],
            )
            .map(|_| ())
        })
    }

    pub fn get(&self, id: &str) -> anyhow::Result<Option<SearchCluster>> {
        self.with(|x| x.query_row(&format!("SELECT {COLS} FROM search_clusters WHERE id = ?1"), [id], row).optional())
    }

    pub fn by_name(&self, name: &str) -> anyhow::Result<Option<SearchCluster>> {
        self.with(|x| x.query_row(&format!("SELECT {COLS} FROM search_clusters WHERE name = ?1"), [name], row).optional())
    }

    pub fn list(&self) -> anyhow::Result<Vec<SearchCluster>> {
        self.with(|x| {
            let mut st = x.prepare(&format!("SELECT {COLS} FROM search_clusters ORDER BY created_at, rowid"))?;
            st.query_map([], row)?.collect()
        })
    }

    /// Clusters branched from `id`.
    pub fn children(&self, id: &str) -> anyhow::Result<Vec<SearchCluster>> {
        self.with(|x| {
            let mut st = x.prepare(&format!("SELECT {COLS} FROM search_clusters WHERE parent_id = ?1 ORDER BY created_at, rowid"))?;
            st.query_map([id], row)?.collect()
        })
    }

    pub fn delete(&self, id: &str) -> anyhow::Result<()> {
        self.with(|x| {
            x.execute("DELETE FROM search_clusters WHERE id = ?1", [id])?;
            x.execute("DELETE FROM search_usage WHERE cluster_id = ?1", [id])?;
            x.execute("DELETE FROM search_storage WHERE cluster_id = ?1", [id]).map(|_| ())
        })
    }

    /// The latest figures metering saw. A change of `stored_bytes` is logged at `at` for the
    /// storage integral.
    pub fn set_stats(&self, id: &str, at: i64, indices: i64, docs: i64, stored_bytes: i64) -> anyhow::Result<()> {
        self.with(|x| {
            x.execute("UPDATE search_clusters SET indices = ?2, docs = ?3, stored_bytes = ?4 WHERE id = ?1", params![id, indices, docs, stored_bytes])?;
            let last: Option<i64> = x
                .query_row("SELECT bytes FROM search_storage WHERE cluster_id = ?1 ORDER BY at DESC, rowid DESC LIMIT 1", [id], |r| r.get(0))
                .optional()?;
            if last != Some(stored_bytes) {
                x.execute("INSERT INTO search_storage (cluster_id, at, bytes) VALUES (?1, ?2, ?3)", params![id, at, stored_bytes])?;
            }
            Ok(())
        })
    }

    pub fn record_usage(&self, id: &str, u: &UsageSample) -> anyhow::Result<()> {
        self.with(|x| {
            x.execute(
                "INSERT INTO search_usage (cluster_id, at, seconds, loaded_bytes, requests) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, u.at, u.seconds, u.loaded_bytes, u.requests],
            )
            .map(|_| ())
        })
    }

    /// Totals over `[since, now]`.
    pub fn usage_since(&self, id: &str, since: i64, now: i64) -> anyhow::Result<UsageTotals> {
        let mut t = self.with(|x| {
            x.query_row(
                "SELECT COALESCE(SUM(seconds), 0), COALESCE(SUM(CAST(seconds AS REAL) * loaded_bytes), 0.0), COALESCE(SUM(requests), 0),
                        COALESCE(MAX(loaded_bytes), 0)
                 FROM search_usage WHERE cluster_id = ?1 AND at >= ?2",
                params![id, since],
                |r| {
                    let byte_seconds: f64 = r.get(1)?;
                    Ok(UsageTotals {
                        loaded_seconds: r.get(0)?,
                        loaded_gib_hours: byte_seconds / GIB / 3600.0,
                        requests: r.get(2)?,
                        peak_loaded_bytes: r.get(3)?,
                        stored_gib_months: 0.0,
                    })
                },
            )
        })?;
        t.stored_gib_months = self.storage_byte_seconds(id, since, now)? / GIB / MONTH_SECONDS;
        Ok(t)
    }

    /// The integral of stored bytes over `[since, now]`: the size in effect at `since` (the last
    /// change before it) and every change after.
    pub fn storage_byte_seconds(&self, id: &str, since: i64, now: i64) -> anyhow::Result<f64> {
        let points: Vec<(i64, i64)> = self.with(|x| {
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
        })?;
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

    fn cluster(id: &str, name: &str) -> SearchCluster {
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

    #[test]
    fn crud() {
        let s = Store::memory().unwrap();
        let c = cluster("srch_a", "app");
        s.insert(&c).unwrap();
        assert!(s.insert(&c).is_err(), "ids are unique");
        assert!(s.insert(&cluster("srch_b", "app")).is_err(), "names are unique");
        assert_eq!(s.by_name("app").unwrap().unwrap().id, "srch_a");
        let mut child = cluster("srch_c", "app-dev");
        child.parent_id = Some("srch_a".into());
        child.parent_at = Some(5);
        s.insert(&child).unwrap();
        assert_eq!(s.children("srch_a").unwrap().len(), 1);
        let mut c = s.get("srch_a").unwrap().unwrap();
        c.flavor = "opensearch".into();
        c.password = "new".into();
        c.max_in_flight = 8;
        s.update(&c).unwrap();
        let back = s.get("srch_a").unwrap().unwrap();
        assert_eq!((back.flavor.as_str(), back.password.as_str(), back.max_in_flight), ("opensearch", "new", 8));
        assert_eq!(s.list().unwrap().iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["app", "app-dev"]);
        s.delete("srch_a").unwrap();
        assert!(s.get("srch_a").unwrap().is_none());
    }

    #[test]
    fn usage_totals() {
        let s = Store::memory().unwrap();
        s.insert(&cluster("srch_a", "app")).unwrap();
        s.record_usage("srch_a", &UsageSample { at: 100, seconds: 60, loaded_bytes: 1 << 30, requests: 10 }).unwrap();
        s.record_usage("srch_a", &UsageSample { at: 160, seconds: 30, loaded_bytes: 2 << 30, requests: 5 }).unwrap();
        let t = s.usage_since("srch_a", 0, 200).unwrap();
        assert_eq!((t.loaded_seconds, t.requests, t.peak_loaded_bytes), (90, 15, 2 << 30));
        assert!((t.loaded_gib_hours - (60.0 + 60.0) / 3600.0).abs() < 1e-12);
        assert_eq!(s.usage_since("srch_a", 150, 200).unwrap().requests, 5);
        s.delete("srch_a").unwrap();
        assert_eq!(s.usage_since("srch_a", 0, 200).unwrap().requests, 0);
    }

    #[test]
    fn storage_is_a_step_function() {
        let s = Store::memory().unwrap();
        s.insert(&cluster("srch_a", "app")).unwrap();
        s.set_stats("srch_a", 100, 1, 10, 1000).unwrap();
        // An unchanged size logs nothing.
        s.set_stats("srch_a", 150, 1, 12, 1000).unwrap();
        s.set_stats("srch_a", 200, 2, 20, 3000).unwrap();
        assert_eq!(s.get("srch_a").unwrap().unwrap().docs, 20);
        // 100..200 at 1000, 200..300 at 3000.
        assert_eq!(s.storage_byte_seconds("srch_a", 0, 300).unwrap(), 100.0 * 1000.0 + 100.0 * 3000.0);
        // A window starting mid-step uses the size in effect then.
        assert_eq!(s.storage_byte_seconds("srch_a", 150, 300).unwrap(), 50.0 * 1000.0 + 100.0 * 3000.0);
        assert_eq!(s.storage_byte_seconds("srch_a", 250, 300).unwrap(), 50.0 * 3000.0);
        let months = s.usage_since("srch_a", 0, 300).unwrap().stored_gib_months;
        assert!((months - 400_000.0 / GIB / MONTH_SECONDS).abs() < 1e-18);
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

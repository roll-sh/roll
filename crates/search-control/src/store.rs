//! Control-plane state for roll search, through `code-sql`: SQLite (`search.db`, one node) or
//! the system database's Postgres (schema `search`) that every node of a fleet shares. SQL is
//! written once, in SQLite's dialect.
//!
//! - `clusters`: one row per cluster (name, dialect, password, limits, branch origin) plus the
//!   last figures metering saw (indices, documents, stored bytes), so listing clusters never
//!   wakes them or touches object storage. A cluster being deleted keeps its row, flagged
//!   `deleting`, until its data is gone; lookups don't see it. On a fleet every change to a
//!   cluster is made on its owner (`crate::cluster::on_owner`), which applies it to the loaded
//!   cluster itself: no node has to follow the catalog.
//! - `usage`: one row per metering interval of a *loaded* cluster: loaded seconds, the
//!   estimated resident bytes, requests. An idle cluster adds no rows.
//! - `storage`: a change log of each cluster's stored bytes. Storage accrues while a cluster
//!   sleeps too, so instead of a row per cluster per minute the log gets a row only when the
//!   size changes, and GB-months are the integral of that step function over the window.
//!
//! The schema is created with `CREATE … IF NOT EXISTS` (`Db::ensure_schema`: on Postgres under
//! an advisory lock, since nodes start together); it reads the `search.db` files the earlier
//! catalog wrote.
//!
//! Calls are async for their callers; `code-sql` runs them synchronously (SQLite inline, in
//! microseconds; Postgres on a runtime of its own).

use code_sql::{Db, OptionalExtension, Row, params};
use serde::Serialize;
use std::path::Path;

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
    /// Of `stored_bytes`, what a branch's commit points reference that its parent's latest
    /// don't: what the branch adds (all of it for a cluster that isn't a branch).
    pub own_bytes: i64,
    /// On a fleet, the node it's loaded on (set as it loads, cleared as it unloads there): what
    /// lists show without asking the cluster's lease.
    #[serde(skip)]
    pub loaded_on: Option<String>,
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
    db: Db,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS search_clusters (
  id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, region TEXT NOT NULL, flavor TEXT NOT NULL,
  password TEXT NOT NULL, idle_timeout_s INTEGER NOT NULL, max_in_flight INTEGER NOT NULL,
  max_body_mb INTEGER NOT NULL, destructive_requires_name INTEGER NOT NULL,
  parent_id TEXT, parent_at INTEGER, created_at INTEGER NOT NULL, password_rotated_at INTEGER,
  indices INTEGER NOT NULL DEFAULT 0, docs INTEGER NOT NULL DEFAULT 0, stored_bytes INTEGER NOT NULL DEFAULT 0,
  deleting INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS search_clusters_parent ON search_clusters (parent_id);
CREATE INDEX IF NOT EXISTS search_clusters_always_on ON search_clusters (created_at) WHERE idle_timeout_s = 0;
CREATE TABLE IF NOT EXISTS search_usage (
  cluster_id TEXT NOT NULL, at INTEGER NOT NULL, seconds INTEGER NOT NULL, loaded_bytes INTEGER NOT NULL,
  requests INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS search_usage_cluster ON search_usage (cluster_id, at);
CREATE TABLE IF NOT EXISTS search_storage (cluster_id TEXT NOT NULL, at INTEGER NOT NULL, bytes INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS search_storage_cluster ON search_storage (cluster_id, at);
"#;

const COLS: &str = "id, name, region, flavor, password, idle_timeout_s, max_in_flight, max_body_mb, destructive_requires_name, \
                    parent_id, parent_at, created_at, password_rotated_at, indices, docs, stored_bytes";
/// What lookups read: [`COLS`] and where the cluster is loaded.
const READ_COLS: &str = "id, name, region, flavor, password, idle_timeout_s, max_in_flight, max_body_mb, destructive_requires_name, \
                         parent_id, parent_at, created_at, password_rotated_at, indices, docs, stored_bytes, loaded_on, own_bytes";

fn row(r: &Row) -> code_sql::Result<SearchCluster> {
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
        loaded_on: r.get(16)?,
        own_bytes: r.get(17)?,
    })
}

/// Seconds in the 30-day month the GB-month figures use (AWS bills storage per 730-hour month;
/// a 30-day month keeps `days / 30` exact in the dashboard).
pub const MONTH_SECONDS: f64 = 30.0 * 86_400.0;
const GIB: f64 = (1u64 << 30) as f64;

impl Store {
    /// `search.db` at `path` (a fresh Postgres schema instead when `code-sql`'s test database
    /// variable is set, so the tests run on both).
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        Self::with_db(Db::open_or_test(&code_sql::Target::Sqlite(path.to_path_buf()), "search")?)
    }

    /// A catalog of its own in a temporary file (tests; nodes of one process share it).
    pub fn memory() -> anyhow::Result<Self> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("roll-search-catalog-{}-{}-{n}", std::process::id(), crate::meter::now_ms()));
        std::fs::create_dir_all(&dir)?;
        Self::open(&dir.join("search.db"))
    }

    /// The catalog every node of a fleet shares: schema `search` of the system database.
    pub fn postgres(url: &str, opts: code_sql::PgOptions) -> anyhow::Result<Self> {
        Self::with_db(Db::connect(url, "search", opts)?)
    }

    fn with_db(db: Db) -> anyhow::Result<Self> {
        db.ensure_schema(SCHEMA)?;
        db.add_column("search_clusters", "loaded_on", "TEXT")?;
        db.add_column("search_clusters", "own_bytes", "INTEGER NOT NULL DEFAULT 0")?;
        // Catalogs written before `code-sql` lack nothing but may lack the index on parents
        // (created above); their extra columns (`seq`) and tables (`search_tombstones`) are unused.
        Ok(Self { db })
    }

    /// Inserts a new cluster; `false` when its name (or id) is taken.
    pub async fn insert(&self, c: &SearchCluster) -> anyhow::Result<bool> {
        let r = self.db.write(|x| {
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
        });
        match r {
            Ok(_) => Ok(true),
            Err(e) if e.is_unique_violation() => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Saves the settings and the password (not the metering figures: see [`Store::set_stats`]).
    pub async fn update(&self, c: &SearchCluster) -> anyhow::Result<()> {
        self.db.write(|x| {
            x.execute(
                "UPDATE search_clusters SET flavor = ?2, password = ?3, idle_timeout_s = ?4, max_in_flight = ?5, max_body_mb = ?6,
                 destructive_requires_name = ?7, password_rotated_at = ?8 WHERE id = ?1",
                params![c.id, c.flavor, c.password, c.idle_timeout_s, c.max_in_flight, c.max_body_mb, c.destructive_requires_name, c.password_rotated_at],
            )
        })?;
        Ok(())
    }

    async fn one(&self, filter: &str, arg: &str) -> anyhow::Result<Option<SearchCluster>> {
        Ok(self.db.read(|x| x.query_row(&format!("SELECT {READ_COLS} FROM search_clusters WHERE {filter} = ?1 AND deleting = 0"), params![arg], row).optional())?)
    }

    pub async fn get(&self, id: &str) -> anyhow::Result<Option<SearchCluster>> {
        self.one("id", id).await
    }

    /// The cluster named `name`, unless it is being deleted.
    pub async fn by_name(&self, name: &str) -> anyhow::Result<Option<SearchCluster>> {
        self.one("name", name).await
    }

    async fn many(&self, filter: &str, arg: Option<&str>) -> anyhow::Result<Vec<SearchCluster>> {
        let sql = format!("SELECT {READ_COLS} FROM search_clusters WHERE deleting = 0 {filter} ORDER BY created_at, rowid");
        Ok(self.db.read(|x| match arg {
            Some(a) => x.query(&sql, params![a], row),
            None => x.query(&sql, params![], row),
        })?)
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

    /// Records that cluster `name` is loaded on `node` (`Some`), or no longer there (`None`:
    /// cleared only if it still names `me`, so a late unload never erases the next owner's).
    pub async fn set_loaded_on(&self, name: &str, node: Option<&str>, me: &str) -> anyhow::Result<()> {
        self.db.write(|x| match node {
            Some(n) => x.execute("UPDATE search_clusters SET loaded_on = ?2 WHERE name = ?1", params![name, n]),
            None => x.execute("UPDATE search_clusters SET loaded_on = NULL WHERE name = ?1 AND loaded_on = ?2", params![name, me]),
        })?;
        Ok(())
    }

    /// Hides a cluster from lookups while its data is deleted (`on`), or shows it again (the
    /// delete failed).
    pub async fn set_deleting(&self, id: &str, on: bool) -> anyhow::Result<()> {
        self.db.write(|x| x.execute("UPDATE search_clusters SET deleting = ?2 WHERE id = ?1", params![id, on]))?;
        Ok(())
    }

    /// Deletes a cluster's rows.
    pub async fn delete(&self, id: &str) -> anyhow::Result<()> {
        self.db.write(|x| {
            x.execute("DELETE FROM search_clusters WHERE id = ?1", params![id])?;
            x.execute("DELETE FROM search_usage WHERE cluster_id = ?1", params![id])?;
            x.execute("DELETE FROM search_storage WHERE cluster_id = ?1", params![id])
        })?;
        Ok(())
    }

    /// The latest figures metering saw. A change of `stored_bytes` is logged at `at` for the
    /// storage integral (one row per change: the update locks the cluster's row first, so nodes
    /// metering it at once during a handoff take turns).
    pub async fn set_stats(&self, id: &str, at: i64, indices: i64, docs: i64, stored_bytes: i64) -> anyhow::Result<()> {
        self.db.write(|x| {
            x.execute("UPDATE search_clusters SET indices = ?2, docs = ?3, stored_bytes = ?4 WHERE id = ?1", params![id, indices, docs, stored_bytes])?;
            let last: Option<i64> =
                x.query_row("SELECT bytes FROM search_storage WHERE cluster_id = ?1 ORDER BY at DESC, rowid DESC LIMIT 1", params![id], |r| r.get(0)).optional()?;
            if last != Some(stored_bytes) {
                x.execute("INSERT INTO search_storage (cluster_id, at, bytes) VALUES (?1, ?2, ?3)", params![id, at, stored_bytes])?;
            }
            Ok::<_, code_sql::Error>(())
        })?;
        Ok(())
    }

    /// [`SearchCluster::own_bytes`], as metering last saw it.
    pub async fn set_own_bytes(&self, id: &str, own_bytes: i64) -> anyhow::Result<()> {
        self.db.write(|x| x.execute("UPDATE search_clusters SET own_bytes = ?2 WHERE id = ?1", params![id, own_bytes]).map(drop))?;
        Ok(())
    }

    pub async fn record_usage(&self, id: &str, u: &UsageSample) -> anyhow::Result<()> {
        self.db.write(|x| {
            x.execute(
                "INSERT INTO search_usage (cluster_id, at, seconds, loaded_bytes, requests) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, u.at, u.seconds, u.loaded_bytes, u.requests],
            )
        })?;
        Ok(())
    }

    /// Totals over `[since, now]`.
    pub async fn usage_since(&self, id: &str, since: i64, now: i64) -> anyhow::Result<UsageTotals> {
        let (seconds, byte_seconds, requests, peak): (i64, f64, i64, i64) = self.db.read(|x| {
            x.query_row(
                "SELECT COALESCE(SUM(seconds), 0), COALESCE(SUM(CAST(seconds AS REAL) * loaded_bytes), 0.0), COALESCE(SUM(requests), 0),
                        COALESCE(MAX(loaded_bytes), 0)
                 FROM search_usage WHERE cluster_id = ?1 AND at >= ?2",
                params![id, since],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
        })?;
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
        let points: Vec<(i64, i64)> = self.db.read(|x| {
            let before: Option<i64> = x
                .query_row("SELECT bytes FROM search_storage WHERE cluster_id = ?1 AND at < ?2 ORDER BY at DESC, rowid DESC LIMIT 1", params![id, since], |r| r.get(0))
                .optional()?;
            let mut v: Vec<(i64, i64)> = before.map(|b| (since, b)).into_iter().collect();
            v.extend(x.query("SELECT at, bytes FROM search_storage WHERE cluster_id = ?1 AND at >= ?2 AND at <= ?3 ORDER BY at, rowid", params![id, since, now], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?);
            Ok::<_, code_sql::Error>(v)
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
            own_bytes: 0,
            loaded_on: None,
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
        usage_totals(s).await;
        storage_is_a_step_function(s).await;
    }

    #[tokio::test]
    async fn catalog() {
        // SQLite, or Postgres when code-sql's test database variable names one.
        all(&Store::memory().unwrap()).await;
    }

    /// A `search.db` the earlier catalog wrote (its own migrations, change counters) opens and
    /// keeps its clusters.
    #[tokio::test]
    async fn opens_an_earlier_catalog() {
        let dir = std::env::temp_dir().join(format!("search-control-store-{}-{}", std::process::id(), crate::meter::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("search.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE search_clusters (
                  id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, region TEXT NOT NULL, flavor TEXT NOT NULL,
                  password TEXT NOT NULL, idle_timeout_s INTEGER NOT NULL, max_in_flight INTEGER NOT NULL,
                  max_body_mb INTEGER NOT NULL, destructive_requires_name INTEGER NOT NULL,
                  parent_id TEXT, parent_at INTEGER, created_at INTEGER NOT NULL, password_rotated_at INTEGER,
                  indices INTEGER NOT NULL DEFAULT 0, docs INTEGER NOT NULL DEFAULT 0, stored_bytes INTEGER NOT NULL DEFAULT 0,
                  seq INTEGER NOT NULL DEFAULT 0, deleting INTEGER NOT NULL DEFAULT 0);
                 CREATE TABLE search_tombstones (name TEXT NOT NULL, seq INTEGER NOT NULL, at INTEGER NOT NULL);
                 PRAGMA user_version = 3;",
            )
            .unwrap();
            conn.execute(&format!("INSERT INTO search_clusters ({COLS}) VALUES ('srch_o', 'old', 'local', 'elasticsearch', 'pw', 300, 64, 100, 1, NULL, NULL, 1, NULL, 0, 0, 0)"), []).unwrap();
        }
        if std::env::var(code_sql::TEST_DATABASE_ENV).is_ok_and(|v| !v.is_empty()) {
            return;
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.by_name("old").await.unwrap().unwrap().id, "srch_o");
        s.delete("srch_o").await.unwrap();
        all(&s).await;
        let _ = std::fs::remove_dir_all(&dir);
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

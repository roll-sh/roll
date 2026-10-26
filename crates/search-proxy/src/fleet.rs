//! Fleet membership: which search nodes are running, where, and whether they take new work.
//!
//! Every node heartbeats one record (`Beat`: its id, the addresses of its Elasticsearch REST and
//! control API listeners, its availability zone, whether it is draining, how many clusters it
//! has loaded). Proxies list the records and treat a node as gone once its record hasn't been
//! refreshed for the TTL. One record per node: liveness costs nothing per cluster.
//!
//! Ages are computed by the store's clock (Postgres `now()`, or the one host's clock for
//! SQLite), so node and proxy clocks don't have to agree.
//!
//! Stores: [`MemoryMembers`] (tests, one process), [`SqliteMembers`] (one host: a node and a
//! proxy sharing a file), [`PgMembers`] (a fleet: roll postgres or RDS, the same database as
//! the ownership leases). Records of nodes that vanished without leaving are pruned after
//! [`PRUNE_AFTER`].
//!
//! The node side is [`Heartbeat`]: it beats at once when started (so proxies route to a node as
//! soon as it listens), then every period; [`Heartbeat::drain`] publishes `draining` right away
//! (SIGTERM: proxies stop sending new work), and [`Heartbeat::stop`] removes the record.

use crate::pg::Pg;
use async_trait::async_trait;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// Records not refreshed for this long are deleted (nodes that died without leaving).
pub const PRUNE_AFTER: Duration = Duration::from_secs(3600);

/// What a node publishes about itself.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Beat {
    /// Unique per running node; also its identity in ownership leases.
    pub id: String,
    /// `host:port` of its Elasticsearch REST listener, as proxies reach it.
    pub http: String,
    /// `host:port` of its control API (`/v1/search`), if it serves one.
    pub api: Option<String>,
    pub az: Option<String>,
    /// Shutting down: serves what it holds, takes nothing new.
    pub draining: bool,
    /// Clusters loaded right now.
    pub loaded: u32,
    /// The REST listener serves TLS with the certificate of this SHA-256 (hex): proxies connect
    /// with TLS and accept only it ([`crate::upstream`]). `None`: plain HTTP.
    pub tls: Option<String>,
    /// The control API listener serves TLS with the same certificate (needs `tls`). A node
    /// whose API port is shared with other services (all of rolld) keeps it plain.
    pub api_tls: bool,
}

impl Beat {
    /// The certificate each of the node's addresses must present (`Err`: a malformed pin, and a
    /// node that asks for TLS is never reached without it).
    pub fn pins(&self) -> Result<Vec<(std::net::SocketAddr, crate::upstream::CertPin)>, String> {
        let Some(t) = &self.tls else { return Ok(Vec::new()) };
        let pin = crate::upstream::parse_pin(t).ok_or_else(|| format!("bad certificate pin {t:?}"))?;
        let mut v = Vec::with_capacity(2);
        if let Ok(a) = self.http.parse() {
            v.push((a, pin));
        }
        if self.api_tls
            && let Some(a) = self.api.as_deref().and_then(|a| a.parse().ok())
        {
            v.push((a, pin));
        }
        Ok(v)
    }
}

/// A node as a store last saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRecord {
    pub beat: Beat,
    /// Milliseconds since its last heartbeat, by the store's clock.
    pub age_ms: u64,
}

#[async_trait]
pub trait Members: Send + Sync + 'static {
    /// Publishes (inserts or refreshes) a node's record.
    async fn beat(&self, b: &Beat) -> anyhow::Result<()>;
    /// Removes a node's record (it stopped).
    async fn leave(&self, id: &str) -> anyhow::Result<()>;
    /// Every record, with its age.
    async fn list(&self) -> anyhow::Result<Vec<NodeRecord>>;
}

/// Records in memory (tests, one process).
#[derive(Default)]
pub struct MemoryMembers {
    rows: Mutex<HashMap<String, (Beat, Instant)>>,
}

impl MemoryMembers {
    pub fn new() -> Arc<MemoryMembers> {
        Arc::new(MemoryMembers::default())
    }
}

#[async_trait]
impl Members for MemoryMembers {
    async fn beat(&self, b: &Beat) -> anyhow::Result<()> {
        let mut rows = self.rows.lock();
        rows.retain(|_, (_, at)| at.elapsed() < PRUNE_AFTER);
        rows.insert(b.id.clone(), (b.clone(), Instant::now()));
        Ok(())
    }

    async fn leave(&self, id: &str) -> anyhow::Result<()> {
        self.rows.lock().remove(id);
        Ok(())
    }

    async fn list(&self) -> anyhow::Result<Vec<NodeRecord>> {
        let mut v: Vec<NodeRecord> =
            self.rows.lock().values().map(|(b, at)| NodeRecord { beat: b.clone(), age_ms: at.elapsed().as_millis() as u64 }).collect();
        v.sort_by(|a, b| a.beat.id.cmp(&b.beat.id));
        Ok(v)
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

const SQLITE_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS search_nodes (
    id TEXT PRIMARY KEY, http TEXT NOT NULL, api TEXT, az TEXT, draining INTEGER NOT NULL,
    loaded INTEGER NOT NULL, beat_ms INTEGER NOT NULL, tls TEXT, api_tls INTEGER NOT NULL DEFAULT 0)";

/// Columns added after the first release, with their definitions (files made by older nodes
/// gain them in place).
const SQLITE_ADDED: [(&str, &str); 2] = [("tls", "TEXT"), ("api_tls", "INTEGER NOT NULL DEFAULT 0")];

/// Records in a SQLite file (a node and a proxy on one host).
pub struct SqliteMembers {
    conn: Arc<Mutex<rusqlite::Connection>>,
}

impl SqliteMembers {
    pub fn open(path: &Path) -> anyhow::Result<Arc<SqliteMembers>> {
        let conn = rusqlite::Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SQLITE_SCHEMA)?;
        for (col, def) in SQLITE_ADDED {
            let has = conn.prepare("SELECT 1 FROM pragma_table_info('search_nodes') WHERE name = ?1")?.exists([col])?;
            if !has && let Err(e) = conn.execute_batch(&format!("ALTER TABLE search_nodes ADD COLUMN {col} {def}")) {
                // Another process sharing the file added it first.
                anyhow::ensure!(e.to_string().contains("duplicate column"), e);
            }
        }
        Ok(Arc::new(SqliteMembers { conn: Arc::new(Mutex::new(conn)) }))
    }

    async fn run<T: Send + 'static>(&self, f: impl FnOnce(&rusqlite::Connection) -> anyhow::Result<T> + Send + 'static) -> anyhow::Result<T> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || f(&conn.lock())).await?
    }
}

#[async_trait]
impl Members for SqliteMembers {
    async fn beat(&self, b: &Beat) -> anyhow::Result<()> {
        let b = b.clone();
        self.run(move |c| {
            let now = now_ms();
            c.execute(
                "INSERT INTO search_nodes (id, http, api, az, draining, loaded, beat_ms, tls, api_tls) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT (id) DO UPDATE SET http = excluded.http, api = excluded.api, az = excluded.az,
                   draining = excluded.draining, loaded = excluded.loaded, beat_ms = excluded.beat_ms, tls = excluded.tls,
                   api_tls = excluded.api_tls",
                rusqlite::params![b.id, b.http, b.api, b.az, b.draining, b.loaded, now, b.tls, b.api_tls],
            )?;
            c.execute("DELETE FROM search_nodes WHERE beat_ms < ?1", [now - PRUNE_AFTER.as_millis() as i64])?;
            Ok(())
        })
        .await
    }

    async fn leave(&self, id: &str) -> anyhow::Result<()> {
        let id = id.to_string();
        self.run(move |c| {
            c.execute("DELETE FROM search_nodes WHERE id = ?1", [id])?;
            Ok(())
        })
        .await
    }

    async fn list(&self) -> anyhow::Result<Vec<NodeRecord>> {
        self.run(|c| {
            let now = now_ms();
            let mut st = c.prepare_cached("SELECT id, http, api, az, draining, loaded, beat_ms, tls, api_tls FROM search_nodes ORDER BY id")?;
            let rows = st.query_map([], |r| {
                Ok(NodeRecord {
                    beat: Beat { id: r.get(0)?, http: r.get(1)?, api: r.get(2)?, az: r.get(3)?, draining: r.get(4)?, loaded: r.get(5)?,
                        tls: r.get(7)?,
                        api_tls: r.get(8)?,
                    },
                    age_ms: (now - r.get::<_, i64>(6)?).max(0) as u64,
                })
            })?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
    }
}

const PG_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS roll_search_nodes (
    id TEXT PRIMARY KEY, http TEXT NOT NULL, api TEXT, az TEXT, draining BOOLEAN NOT NULL,
    loaded INTEGER NOT NULL, beat TIMESTAMPTZ NOT NULL, tls TEXT, api_tls BOOLEAN NOT NULL DEFAULT false);
ALTER TABLE roll_search_nodes ADD COLUMN IF NOT EXISTS tls TEXT;
ALTER TABLE roll_search_nodes ADD COLUMN IF NOT EXISTS api_tls BOOLEAN NOT NULL DEFAULT false";

/// Records in Postgres; the database's clock decides ages.
pub struct PgMembers {
    pg: Arc<Pg>,
    beats: std::sync::atomic::AtomicU64,
}

impl PgMembers {
    /// Over an open connection (shared with the catalog); creates the table.
    pub async fn new(pg: Arc<Pg>) -> anyhow::Result<Arc<PgMembers>> {
        pg.client().await?.batch_execute(PG_SCHEMA).await?;
        Ok(Arc::new(PgMembers { pg, beats: Default::default() }))
    }

    pub async fn connect(url: &str, ca: Option<&Path>, password: Option<&str>) -> anyhow::Result<Arc<PgMembers>> {
        Self::new(Pg::connect(url, ca, password).await?).await
    }
}

#[async_trait]
impl Members for PgMembers {
    async fn beat(&self, b: &Beat) -> anyhow::Result<()> {
        let c = self.pg.client().await?;
        c.execute(
            "INSERT INTO roll_search_nodes (id, http, api, az, draining, loaded, beat, tls, api_tls) VALUES ($1, $2, $3, $4, $5, $6, now(), $7, $8)
             ON CONFLICT (id) DO UPDATE SET http = EXCLUDED.http, api = EXCLUDED.api, az = EXCLUDED.az,
               draining = EXCLUDED.draining, loaded = EXCLUDED.loaded, beat = EXCLUDED.beat, tls = EXCLUDED.tls,
               api_tls = EXCLUDED.api_tls",
            &[&b.id, &b.http, &b.api, &b.az, &b.draining, &(b.loaded.min(i32::MAX as u32) as i32), &b.tls, &b.api_tls],
        )
        .await?;
        // Pruning is rare work: once a minute of beats is plenty.
        if self.beats.fetch_add(1, std::sync::atomic::Ordering::Relaxed).is_multiple_of(60) {
            c.execute("DELETE FROM roll_search_nodes WHERE beat < now() - $1 * interval '1 second'", &[&(PRUNE_AFTER.as_secs() as f64)]).await?;
        }
        Ok(())
    }

    async fn leave(&self, id: &str) -> anyhow::Result<()> {
        self.pg.client().await?.execute("DELETE FROM roll_search_nodes WHERE id = $1", &[&id]).await?;
        Ok(())
    }

    async fn list(&self) -> anyhow::Result<Vec<NodeRecord>> {
        let rows = self
            .pg
            .client()
            .await?
            .query(
                "SELECT id, http, api, az, draining, loaded, GREATEST(0, EXTRACT(EPOCH FROM (now() - beat)) * 1000)::BIGINT, tls, api_tls
                 FROM roll_search_nodes ORDER BY id",
                &[],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| NodeRecord {
                beat: Beat {
                    id: r.get(0),
                    http: r.get(1),
                    api: r.get(2),
                    az: r.get(3),
                    draining: r.get(4),
                    loaded: r.get::<_, i32>(5).max(0) as u32,
                    tls: r.get(7),
                    api_tls: r.get(8),
                },
                age_ms: r.get::<_, i64>(6).max(0) as u64,
            })
            .collect())
    }
}

/// Opens the membership store at `url` (`postgres://…` or a SQLite path).
pub async fn open(url: &str, ca: Option<&Path>, password: Option<&str>) -> anyhow::Result<Arc<dyn Members>> {
    Ok(match crate::pg::MetaUrl::parse(url) {
        crate::pg::MetaUrl::Postgres(u) => PgMembers::connect(&u, ca, password).await?,
        crate::pg::MetaUrl::Sqlite(p) => SqliteMembers::open(&p)?,
    })
}

/// A node's heartbeat task.
pub struct Heartbeat {
    members: Arc<dyn Members>,
    beat: Mutex<Beat>,
    kick: Arc<Notify>,
    stopped: std::sync::atomic::AtomicBool,
    /// Held across each write of the record: a beat still on its way when the node leaves
    /// would otherwise write the record back after `leave` removed it, and proxies would keep
    /// routing to a node that is gone until the record ages out.
    io: tokio::sync::Mutex<()>,
}

impl Heartbeat {
    /// Beats now, then every `every` until [`Heartbeat::stop`]. Fails if the first beat does
    /// (the store is unreachable: better to fail the start than to run invisible).
    pub async fn start(members: Arc<dyn Members>, beat: Beat, every: Duration) -> anyhow::Result<Arc<Heartbeat>> {
        members.beat(&beat).await?;
        let hb = Arc::new(Heartbeat { members, beat: Mutex::new(beat), kick: Arc::new(Notify::new()), stopped: Default::default(), io: tokio::sync::Mutex::new(()) });
        let weak = Arc::downgrade(&hb);
        tokio::spawn(async move {
            loop {
                let Some(h) = weak.upgrade() else { return };
                let kick = h.kick.clone();
                drop(h);
                // `notify_one` stores a permit, so a kick between here and the wait isn't lost.
                let _ = tokio::time::timeout(every, kick.notified()).await;
                let Some(h) = weak.upgrade() else { return };
                let _io = h.io.lock().await;
                if h.stopped.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                let b = h.beat.lock().clone();
                if let Err(e) = h.members.beat(&b).await {
                    tracing::warn!(error = %e, "search node heartbeat failed");
                }
            }
        });
        Ok(hb)
    }

    pub fn id(&self) -> String {
        self.beat.lock().id.clone()
    }

    pub fn beat(&self) -> Beat {
        self.beat.lock().clone()
    }

    /// The clusters loaded now (published with the next beat).
    pub fn set_loaded(&self, n: u32) {
        self.beat.lock().loaded = n;
    }

    /// Publishes `draining` at once: proxies stop sending new work to this node.
    pub async fn drain(&self) -> anyhow::Result<()> {
        let b = {
            let mut b = self.beat.lock();
            b.draining = true;
            b.clone()
        };
        let _io = self.io.lock().await;
        if self.stopped.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(());
        }
        self.members.beat(&b).await
    }

    /// Stops beating and removes the record.
    pub async fn stop(&self) -> anyhow::Result<()> {
        self.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
        self.kick.notify_one();
        let id = self.id();
        // After any beat in progress: nothing writes the record once this returns.
        let _io = self.io.lock().await;
        self.members.leave(&id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn beat(id: &str, az: &str) -> Beat {
        Beat { id: id.into(), http: "10.0.0.1:9200".into(), api: Some("10.0.0.1:7070".into()), az: Some(az.into()), ..Beat::default() }
    }

    async fn exercise(m: &dyn Members) {
        // Only this test's records: a shared test database may hold other tests' nodes.
        let mine = async || -> Vec<NodeRecord> { m.list().await.unwrap().into_iter().filter(|r| matches!(r.beat.id.as_str(), "a" | "b")).collect() };
        m.beat(&beat("a", "az1")).await.unwrap();
        m.beat(&beat("b", "az2")).await.unwrap();
        let l = mine().await;
        assert_eq!(l.iter().map(|r| r.beat.id.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        assert!(l.iter().all(|r| r.age_ms < 5_000), "{l:?}");
        assert_eq!(l[0].beat, beat("a", "az1"));
        // A beat replaces the record.
        let mut a = beat("a", "az1");
        a.draining = true;
        a.loaded = 7;
        a.api = None;
        a.tls = Some("ab".repeat(32));
        m.beat(&a).await.unwrap();
        let l = mine().await;
        assert_eq!(l[0].beat, a);
        a.api = Some("10.0.0.1:7070".into());
        a.api_tls = true;
        m.beat(&a).await.unwrap();
        assert_eq!(mine().await[0].beat, a);
        m.leave("a").await.unwrap();
        m.leave("nope").await.unwrap();
        assert_eq!(mine().await.iter().map(|r| r.beat.id.as_str()).collect::<Vec<_>>(), ["b"]);
        m.leave("b").await.unwrap();
    }

    #[tokio::test]
    async fn memory() {
        exercise(&*MemoryMembers::new()).await;
    }

    #[tokio::test]
    async fn sqlite() {
        let dir = std::env::temp_dir().join(format!("search-proxy-fleet-{}-{}", std::process::id(), now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fleet.db");
        exercise(&*SqliteMembers::open(&path).unwrap()).await;
        // Two handles on one file see each other (a node and a proxy).
        let (x, y) = (SqliteMembers::open(&path).unwrap(), SqliteMembers::open(&path).unwrap());
        x.beat(&beat("n", "az1")).await.unwrap();
        assert_eq!(y.list().await.unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn sqlite_files_from_before_pins_gain_the_columns() {
        let dir = std::env::temp_dir().join(format!("search-proxy-fleet-old-{}-{}", std::process::id(), now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fleet.db");
        {
            let c = rusqlite::Connection::open(&path).unwrap();
            c.execute_batch(
                "CREATE TABLE search_nodes (id TEXT PRIMARY KEY, http TEXT NOT NULL, api TEXT, az TEXT, draining INTEGER NOT NULL,
                 loaded INTEGER NOT NULL, beat_ms INTEGER NOT NULL)",
            )
            .unwrap();
            c.execute("INSERT INTO search_nodes VALUES ('old', 'h:1', NULL, NULL, 0, 0, ?1)", [now_ms()]).unwrap();
        }
        let m = SqliteMembers::open(&path).unwrap();
        // Reopening an upgraded file is a no-op.
        let again = SqliteMembers::open(&path).unwrap();
        let l = again.list().await.unwrap();
        assert_eq!((l.len(), l[0].beat.tls.clone(), l[0].beat.api_tls), (1, None, false));
        let mut b = beat("new", "az1");
        b.tls = Some("cd".repeat(32));
        b.api_tls = true;
        m.beat(&b).await.unwrap();
        assert_eq!(again.list().await.unwrap().iter().find(|r| r.beat.id == "new").unwrap().beat, b);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pins_cover_the_api_only_when_it_serves_tls() {
        let pin = "ab".repeat(32);
        let mut b = Beat { id: "n".into(), http: "10.0.0.1:9200".into(), api: Some("10.0.0.1:7070".into()), ..Beat::default() };
        assert_eq!(b.pins(), Ok(vec![]));
        b.tls = Some(pin.clone());
        let p = crate::upstream::parse_pin(&pin).unwrap();
        assert_eq!(b.pins(), Ok(vec![("10.0.0.1:9200".parse().unwrap(), p)]));
        b.api_tls = true;
        assert_eq!(b.pins(), Ok(vec![("10.0.0.1:9200".parse().unwrap(), p), ("10.0.0.1:7070".parse().unwrap(), p)]));
        b.tls = Some("nope".into());
        assert!(b.pins().is_err());
    }

    /// Runs when `ROLL_SEARCH_TEST_PG` names a Postgres URL.
    #[tokio::test]
    async fn postgres() {
        let Ok(url) = std::env::var("ROLL_SEARCH_TEST_PG") else { return };
        let m = PgMembers::connect(&url, None, None).await.unwrap();
        m.pg.client().await.unwrap().execute("DELETE FROM roll_search_nodes WHERE id IN ('a', 'b')", &[]).await.unwrap();
        exercise(&*m).await;
    }

    #[tokio::test]
    async fn heartbeats_drain_and_leave() {
        let m = MemoryMembers::new();
        let hb = Heartbeat::start(m.clone(), beat("n", "az1"), Duration::from_millis(20)).await.unwrap();
        assert_eq!(m.list().await.unwrap().len(), 1, "beats before returning");
        hb.set_loaded(3);
        tokio::time::sleep(Duration::from_millis(80)).await;
        let r = &m.list().await.unwrap()[0];
        assert_eq!(r.beat.loaded, 3);
        assert!(r.age_ms < 60, "{r:?}");
        hb.drain().await.unwrap();
        assert!(m.list().await.unwrap()[0].beat.draining);
        hb.stop().await.unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(m.list().await.unwrap().is_empty(), "no beat after stop");
    }

    /// Records whose writes take a while (a busy metadata store): a beat lands after it began.
    struct Slow {
        inner: Arc<MemoryMembers>,
        beating: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl Members for Slow {
        async fn beat(&self, b: &Beat) -> anyhow::Result<()> {
            self.beating.store(true, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(150)).await;
            self.inner.beat(b).await
        }
        async fn leave(&self, id: &str) -> anyhow::Result<()> {
            self.inner.leave(id).await
        }
        async fn list(&self) -> anyhow::Result<Vec<NodeRecord>> {
            self.inner.list().await
        }
    }

    #[tokio::test]
    async fn a_beat_in_progress_never_outlives_leaving() {
        let inner = MemoryMembers::new();
        let m = Arc::new(Slow { inner: inner.clone(), beating: Default::default() });
        let hb = Heartbeat::start(m.clone(), beat("n", "az1"), Duration::from_millis(10)).await.unwrap();
        // Leave while the next beat is on its way to the store.
        m.beating.store(false, std::sync::atomic::Ordering::SeqCst);
        while !m.beating.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        hb.stop().await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(inner.list().await.unwrap().is_empty(), "the record came back after leaving");
    }
}

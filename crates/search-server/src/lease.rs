//! Cluster ownership for nodes sharing a bucket: one owner per cluster at a time, through a
//! lease with an epoch in a metadata store.
//!
//! - **Acquire** takes a free or expired lease (bumping the epoch) or renews one this node
//!   holds (same epoch); a lease another node holds and hasn't let expire is refused with its
//!   owner, which the front proxy routes to.
//! - **Epochs** only grow, per cluster, also across releases, and are what search-store writes
//!   into manifests and cluster states: once a newer owner wrote anything, an older one's writes
//!   fail (`Fenced`).
//! - **Acknowledgements** need the lease: the node renews every `ttl / 6` and computes a local
//!   deadline from the instant it *sent* a successful acquire or renewal (`sent + ttl - margin`),
//!   which is never later than the store's expiry. Writes are only acknowledged before it, so
//!   a node that loses contact stops acknowledging before another node can take over.
//! - Stores: [`MemoryLeases`] (tests, one process), [`SqliteLeases`] (one host),
//!   [`PgLeases`] (roll postgres / RDS for a fleet; the database's clock decides expiry, so
//!   node clocks don't have to agree; TLS per the URL's `sslmode`, certificates verified).
//! - **Renewal is batched** ([`Leases::renew_many`]): a node holding thousands of clusters
//!   renews them all in one statement (Postgres) or one transaction (SQLite) per period.

use async_trait::async_trait;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// Who holds a lease.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Owner {
    pub node: String,
    pub epoch: u64,
    /// Milliseconds until it expires as the store sees it (0: expired or released).
    pub remaining_ms: u64,
}

#[async_trait]
pub trait Leases: Send + Sync + 'static {
    /// Takes `cluster`'s lease for `node` until `ttl` from now (or renews it). `Ok(Ok(epoch))`:
    /// held under `epoch`; `Ok(Err(owner))`: another node holds it.
    async fn acquire(&self, cluster: &str, node: &str, ttl: Duration) -> anyhow::Result<Result<u64, Owner>>;
    /// Extends a lease held under `epoch`; `false` when it is not held any more. A lease that
    /// expired or was released is never extended, even if nobody took it since: a renewal that
    /// raced a release (an unload, a drain) must not keep the next owner out for a whole `ttl`.
    async fn renew(&self, cluster: &str, node: &str, epoch: u64, ttl: Duration) -> anyhow::Result<bool>;
    /// Extends every lease in `held` (`(cluster, epoch)`); answers per entry, in order. The
    /// default renews one by one; stores override it with one round trip.
    async fn renew_many(&self, held: &[(String, u64)], node: &str, ttl: Duration) -> anyhow::Result<Vec<bool>> {
        let mut out = Vec::with_capacity(held.len());
        for (cluster, epoch) in held {
            out.push(self.renew(cluster, node, *epoch, ttl).await?);
        }
        Ok(out)
    }
    /// Gives a lease up (the epoch stays: the next owner gets a higher one).
    async fn release(&self, cluster: &str, node: &str, epoch: u64) -> anyhow::Result<()>;
    async fn owner(&self, cluster: &str) -> anyhow::Result<Option<Owner>>;
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// The decision every store applies, given the current row `(node, epoch, expires_ms)`.
fn decide(row: Option<(&str, u64, u64)>, node: &str, now: u64, ttl: Duration) -> Result<(u64, u64), Owner> {
    let expires = now + ttl.as_millis() as u64;
    match row {
        None => Ok((1, expires)),
        Some((n, e, exp)) if n == node && exp > now => Ok((e, expires)),
        Some((_, e, exp)) if exp <= now => Ok((e + 1, expires)),
        Some((n, e, exp)) => Err(Owner { node: n.to_string(), epoch: e, remaining_ms: exp - now }),
    }
}

/// Leases in memory (one process: tests, a single node).
#[derive(Default)]
pub struct MemoryLeases {
    rows: Mutex<HashMap<String, (String, u64, u64)>>,
}

impl MemoryLeases {
    pub fn new() -> Arc<MemoryLeases> {
        Arc::new(MemoryLeases::default())
    }
}

#[async_trait]
impl Leases for MemoryLeases {
    async fn acquire(&self, cluster: &str, node: &str, ttl: Duration) -> anyhow::Result<Result<u64, Owner>> {
        let mut rows = self.rows.lock();
        let row = rows.get(cluster).map(|(n, e, x)| (n.as_str(), *e, *x));
        Ok(decide(row, node, now_ms(), ttl).map(|(epoch, expires)| {
            rows.insert(cluster.to_string(), (node.to_string(), epoch, expires));
            epoch
        }))
    }

    async fn renew(&self, cluster: &str, node: &str, epoch: u64, ttl: Duration) -> anyhow::Result<bool> {
        let mut rows = self.rows.lock();
        let now = now_ms();
        match rows.get_mut(cluster) {
            Some((n, e, x)) if n == node && *e == epoch && *x > now => {
                *x = now + ttl.as_millis() as u64;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn release(&self, cluster: &str, node: &str, epoch: u64) -> anyhow::Result<()> {
        if let Some((n, e, x)) = self.rows.lock().get_mut(cluster)
            && n == node
            && *e == epoch
        {
            *x = 0;
        }
        Ok(())
    }

    async fn owner(&self, cluster: &str) -> anyhow::Result<Option<Owner>> {
        let now = now_ms();
        Ok(self.rows.lock().get(cluster).map(|(n, e, x)| Owner { node: n.clone(), epoch: *e, remaining_ms: x.saturating_sub(now) }))
    }
}

const SQLITE_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS search_leases (
    cluster TEXT PRIMARY KEY, node TEXT NOT NULL, epoch INTEGER NOT NULL, expires_ms INTEGER NOT NULL)";

/// Leases in a SQLite file (several processes on one host; WAL journal, `BEGIN IMMEDIATE`).
pub struct SqliteLeases {
    conn: Arc<Mutex<rusqlite::Connection>>,
}

impl SqliteLeases {
    pub fn open(path: &Path) -> anyhow::Result<Arc<SqliteLeases>> {
        let conn = rusqlite::Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SQLITE_SCHEMA)?;
        Ok(Arc::new(SqliteLeases { conn: Arc::new(Mutex::new(conn)) }))
    }

    async fn run<T: Send + 'static>(&self, f: impl FnOnce(&mut rusqlite::Connection) -> anyhow::Result<T> + Send + 'static) -> anyhow::Result<T> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || f(&mut conn.lock())).await?
    }
}

fn row(tx: &rusqlite::Transaction<'_>, cluster: &str) -> anyhow::Result<Option<(String, u64, u64)>> {
    use rusqlite::OptionalExtension;
    Ok(tx
        .query_row("SELECT node, epoch, expires_ms FROM search_leases WHERE cluster = ?1", [cluster], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64, r.get::<_, i64>(2)? as u64))
        })
        .optional()?)
}

#[async_trait]
impl Leases for SqliteLeases {
    async fn acquire(&self, cluster: &str, node: &str, ttl: Duration) -> anyhow::Result<Result<u64, Owner>> {
        let (cluster, node) = (cluster.to_string(), node.to_string());
        self.run(move |c| {
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let cur = row(&tx, &cluster)?;
            let d = decide(cur.as_ref().map(|(n, e, x)| (n.as_str(), *e, *x)), &node, now_ms(), ttl);
            if let Ok((epoch, expires)) = d {
                tx.execute(
                    "INSERT INTO search_leases (cluster, node, epoch, expires_ms) VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT (cluster) DO UPDATE SET node = excluded.node, epoch = excluded.epoch, expires_ms = excluded.expires_ms",
                    rusqlite::params![cluster, node, epoch as i64, expires as i64],
                )?;
            }
            tx.commit()?;
            Ok(d.map(|(e, _)| e))
        })
        .await
    }

    async fn renew(&self, cluster: &str, node: &str, epoch: u64, ttl: Duration) -> anyhow::Result<bool> {
        let (cluster, node) = (cluster.to_string(), node.to_string());
        self.run(move |c| {
            let now = now_ms();
            let n = c.execute(
                "UPDATE search_leases SET expires_ms = ?4 WHERE cluster = ?1 AND node = ?2 AND epoch = ?3 AND expires_ms > ?5",
                rusqlite::params![cluster, node, epoch as i64, (now + ttl.as_millis() as u64) as i64, now as i64],
            )?;
            Ok(n == 1)
        })
        .await
    }

    async fn renew_many(&self, held: &[(String, u64)], node: &str, ttl: Duration) -> anyhow::Result<Vec<bool>> {
        let (held, node) = (held.to_vec(), node.to_string());
        self.run(move |c| {
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let now = now_ms();
            let expires = (now + ttl.as_millis() as u64) as i64;
            let mut out = Vec::with_capacity(held.len());
            {
                let mut st = tx.prepare_cached("UPDATE search_leases SET expires_ms = ?4 WHERE cluster = ?1 AND node = ?2 AND epoch = ?3 AND expires_ms > ?5")?;
                for (cluster, epoch) in &held {
                    out.push(st.execute(rusqlite::params![cluster, node, *epoch as i64, expires, now as i64])? == 1);
                }
            }
            tx.commit()?;
            Ok(out)
        })
        .await
    }

    async fn release(&self, cluster: &str, node: &str, epoch: u64) -> anyhow::Result<()> {
        let (cluster, node) = (cluster.to_string(), node.to_string());
        self.run(move |c| {
            c.execute("UPDATE search_leases SET expires_ms = 0 WHERE cluster = ?1 AND node = ?2 AND epoch = ?3", rusqlite::params![cluster, node, epoch as i64])?;
            Ok(())
        })
        .await
    }

    async fn owner(&self, cluster: &str) -> anyhow::Result<Option<Owner>> {
        let cluster = cluster.to_string();
        self.run(move |c| {
            let tx = c.transaction()?;
            let r = row(&tx, &cluster)?;
            let now = now_ms();
            Ok(r.map(|(node, epoch, x)| Owner { node, epoch, remaining_ms: x.saturating_sub(now) }))
        })
        .await
    }
}

const PG_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS roll_search_leases (
    cluster TEXT PRIMARY KEY, node TEXT NOT NULL, epoch BIGINT NOT NULL, expires TIMESTAMPTZ NOT NULL)";

/// Leases in Postgres. One statement per operation; `now()` of the database decides expiry.
/// TLS follows the URL's `sslmode` (`disable`: never; `prefer`, the default: when the server
/// offers it; `require`: always), always verifying the server against the system roots plus
/// an optional CA bundle (RDS). A lost connection is re-established on the next call.
pub struct PgLeases {
    client: tokio::sync::Mutex<tokio_postgres::Client>,
    config: tokio_postgres::Config,
    tls: Option<tokio_postgres_rustls::MakeRustlsConnect>,
}

impl PgLeases {
    /// Connects and creates the table.
    pub async fn connect(url: &str) -> anyhow::Result<Arc<PgLeases>> {
        Self::connect_with(url, None).await
    }

    /// [`PgLeases::connect`], also trusting the CA certificates in `ca` (PEM).
    pub async fn connect_with(url: &str, ca: Option<&Path>) -> anyhow::Result<Arc<PgLeases>> {
        let config: tokio_postgres::Config = url.parse()?;
        let tls = match config.get_ssl_mode() {
            tokio_postgres::config::SslMode::Disable => None,
            _ => Some(tokio_postgres_rustls::MakeRustlsConnect::new(pg_tls(ca)?)),
        };
        let client = Self::open(&config, tls.as_ref()).await?;
        client.batch_execute(PG_SCHEMA).await?;
        Ok(Arc::new(PgLeases { client: tokio::sync::Mutex::new(client), config, tls }))
    }

    async fn open(config: &tokio_postgres::Config, tls: Option<&tokio_postgres_rustls::MakeRustlsConnect>) -> anyhow::Result<tokio_postgres::Client> {
        fn watch<F: std::future::Future<Output = Result<(), tokio_postgres::Error>> + Send + 'static>(conn: F) {
            // The connection task sleeps on its socket: no timer, no wakeups while idle.
            tokio::spawn(async move {
                if let Err(e) = conn.await {
                    tracing::warn!(error = %e, "lease store connection closed");
                }
            });
        }
        Ok(match tls {
            Some(tls) => {
                let (client, conn) = config.connect(tls.clone()).await?;
                watch(conn);
                client
            }
            None => {
                let (client, conn) = config.connect(tokio_postgres::NoTls).await?;
                watch(conn);
                client
            }
        })
    }

    async fn client(&self) -> anyhow::Result<tokio::sync::MutexGuard<'_, tokio_postgres::Client>> {
        let mut c = self.client.lock().await;
        if c.is_closed() {
            *c = Self::open(&self.config, self.tls.as_ref()).await?;
        }
        Ok(c)
    }
}

fn pg_tls(ca: Option<&Path>) -> anyhow::Result<tokio_rustls::rustls::ClientConfig> {
    use anyhow::Context;
    use tokio_rustls::rustls;
    use tokio_rustls::rustls::pki_types::CertificateDer;
    use tokio_rustls::rustls::pki_types::pem::PemObject;
    let mut roots = rustls::RootCertStore::empty();
    for c in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(c);
    }
    if let Some(ca) = ca {
        for c in CertificateDer::pem_file_iter(ca).with_context(|| format!("{}: reading CA certificates", ca.display()))? {
            roots.add(c.with_context(|| format!("{}: bad CA certificate", ca.display()))?)?;
        }
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    Ok(rustls::ClientConfig::builder_with_provider(provider).with_safe_default_protocol_versions()?.with_root_certificates(roots).with_no_client_auth())
}

#[async_trait]
impl Leases for PgLeases {
    async fn acquire(&self, cluster: &str, node: &str, ttl: Duration) -> anyhow::Result<Result<u64, Owner>> {
        let ms = ttl.as_millis() as f64;
        let c = self.client().await?;
        let got = c
            .query_opt(
                "INSERT INTO roll_search_leases AS l (cluster, node, epoch, expires) VALUES ($1, $2, 1, now() + $3 * interval '1 millisecond')
                 ON CONFLICT (cluster) DO UPDATE SET node = EXCLUDED.node, expires = EXCLUDED.expires,
                   epoch = CASE WHEN l.node = EXCLUDED.node AND l.expires > now() THEN l.epoch ELSE l.epoch + 1 END
                 WHERE l.node = EXCLUDED.node OR l.expires <= now()
                 RETURNING epoch",
                &[&cluster, &node, &ms],
            )
            .await?;
        if let Some(r) = got {
            return Ok(Ok(r.get::<_, i64>(0) as u64));
        }
        let r = c
            .query_one(
                "SELECT node, epoch, GREATEST(0, EXTRACT(EPOCH FROM (expires - now())) * 1000)::BIGINT FROM roll_search_leases WHERE cluster = $1",
                &[&cluster],
            )
            .await?;
        Ok(Err(Owner { node: r.get(0), epoch: r.get::<_, i64>(1) as u64, remaining_ms: r.get::<_, i64>(2) as u64 }))
    }

    async fn renew(&self, cluster: &str, node: &str, epoch: u64, ttl: Duration) -> anyhow::Result<bool> {
        let ms = ttl.as_millis() as f64;
        let n = self
            .client()
            .await?
            .execute(
                "UPDATE roll_search_leases SET expires = now() + $4 * interval '1 millisecond' WHERE cluster = $1 AND node = $2 AND epoch = $3 AND expires > now()",
                &[&cluster, &node, &(epoch as i64), &ms],
            )
            .await?;
        Ok(n == 1)
    }

    async fn renew_many(&self, held: &[(String, u64)], node: &str, ttl: Duration) -> anyhow::Result<Vec<bool>> {
        if held.is_empty() {
            return Ok(Vec::new());
        }
        let ms = ttl.as_millis() as f64;
        let clusters: Vec<&str> = held.iter().map(|(c, _)| c.as_str()).collect();
        let epochs: Vec<i64> = held.iter().map(|(_, e)| *e as i64).collect();
        let rows = self
            .client()
            .await?
            .query(
                "UPDATE roll_search_leases AS l SET expires = now() + $4 * interval '1 millisecond'
                 FROM unnest($1::text[], $2::bigint[]) AS h(cluster, epoch)
                 WHERE l.cluster = h.cluster AND l.epoch = h.epoch AND l.node = $3 AND l.expires > now()
                 RETURNING l.cluster",
                &[&clusters, &epochs, &node, &ms],
            )
            .await?;
        let renewed: std::collections::HashSet<String> = rows.iter().map(|r| r.get::<_, String>(0)).collect();
        Ok(held.iter().map(|(c, _)| renewed.contains(c)).collect())
    }

    async fn release(&self, cluster: &str, node: &str, epoch: u64) -> anyhow::Result<()> {
        self.client()
            .await?
            .execute("UPDATE roll_search_leases SET expires = now() WHERE cluster = $1 AND node = $2 AND epoch = $3", &[&cluster, &node, &(epoch as i64)])
            .await?;
        Ok(())
    }

    async fn owner(&self, cluster: &str) -> anyhow::Result<Option<Owner>> {
        let r = self
            .client()
            .await?
            .query_opt(
                "SELECT node, epoch, GREATEST(0, EXTRACT(EPOCH FROM (expires - now())) * 1000)::BIGINT FROM roll_search_leases WHERE cluster = $1",
                &[&cluster],
            )
            .await?;
        Ok(r.map(|r| Owner { node: r.get(0), epoch: r.get::<_, i64>(1) as u64, remaining_ms: r.get::<_, i64>(2) as u64 }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn exercise(l: &dyn Leases) {
        // Long enough that a loaded test machine doesn't expire leases between two steps.
        let ttl = Duration::from_millis(1500);
        assert_eq!(l.acquire("c", "a", ttl).await.unwrap(), Ok(1));
        // Renewing through acquire keeps the epoch; another node is refused.
        assert_eq!(l.acquire("c", "a", ttl).await.unwrap(), Ok(1));
        let owner = l.acquire("c", "b", ttl).await.unwrap().unwrap_err();
        assert_eq!((owner.node.as_str(), owner.epoch), ("a", 1));
        assert!(l.renew("c", "a", 1, ttl).await.unwrap());
        assert!(!l.renew("c", "b", 1, ttl).await.unwrap());
        // Expiry lets another node in, with a higher epoch; the old holder can't renew.
        tokio::time::sleep(ttl + Duration::from_millis(100)).await;
        assert_eq!(l.acquire("c", "b", ttl).await.unwrap(), Ok(2));
        assert!(!l.renew("c", "a", 1, ttl).await.unwrap());
        // Release: the next holder (even the same node) gets a newer epoch at once.
        l.release("c", "b", 2).await.unwrap();
        // A renewal that raced the release doesn't bring the lease back, alone or batched.
        assert!(!l.renew("c", "b", 2, ttl).await.unwrap());
        assert_eq!(l.renew_many(&[("c".to_string(), 2)], "b", ttl).await.unwrap(), vec![false]);
        assert_eq!(l.acquire("c", "b", ttl).await.unwrap(), Ok(3));
        assert_eq!(l.owner("c").await.unwrap().unwrap().epoch, 3);
        assert_eq!(l.owner("nope").await.unwrap(), None);
        // Batched renewal answers per lease: held, wrong epoch, never taken.
        assert_eq!(l.acquire("c2", "b", ttl).await.unwrap(), Ok(1));
        let held = vec![("c".to_string(), 3), ("c2".to_string(), 7), ("nope".to_string(), 1)];
        assert_eq!(l.renew_many(&held, "b", Duration::from_secs(5)).await.unwrap(), vec![true, false, false]);
        assert_eq!(l.renew_many(&[], "b", ttl).await.unwrap(), Vec::<bool>::new());
        // The renewal took: past the old ttl, the lease is still b's.
        tokio::time::sleep(ttl + Duration::from_millis(100)).await;
        assert!(l.acquire("c", "a", ttl).await.unwrap().is_err());
    }

    #[tokio::test]
    async fn memory() {
        exercise(&*MemoryLeases::new()).await;
    }

    #[tokio::test]
    async fn sqlite() {
        let dir = tempfile::tempdir().unwrap();
        let l = SqliteLeases::open(&dir.path().join("leases.db")).unwrap();
        exercise(&*l).await;
        // A second handle (another process) sees the same leases.
        let l2 = SqliteLeases::open(&dir.path().join("leases.db")).unwrap();
        assert_eq!(l2.owner("c").await.unwrap().unwrap().epoch, 3);
    }

    /// Runs when `ROLL_SEARCH_TEST_PG` names a Postgres URL.
    #[tokio::test]
    async fn postgres() {
        let Ok(url) = std::env::var("ROLL_SEARCH_TEST_PG") else { return };
        let l = PgLeases::connect(&url).await.unwrap();
        l.client().await.unwrap().execute("DELETE FROM roll_search_leases WHERE cluster IN ('c', 'c2', 'nope')", &[]).await.unwrap();
        exercise(&*l).await;
        // `sslmode=require` insists on TLS: a server without it is refused, not used in clear.
        if url.contains("sslmode=disable") || !url.contains("sslmode=") {
            let base = url.split('?').next().unwrap();
            if let Ok(plain) = PgLeases::connect(&format!("{base}?sslmode=disable")).await
                && plain.client().await.unwrap().query_one("SHOW ssl", &[]).await.unwrap().get::<_, String>(0) == "off"
            {
                assert!(PgLeases::connect(&format!("{base}?sslmode=require")).await.is_err());
            }
        }
    }
}

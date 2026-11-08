//! The Postgres backend: membership and leases as rows in a `code-sql` database (Postgres in a
//! cluster; SQLite works for nodes sharing one host, as tests do). The tables keep the names
//! roll code's cluster gave them (`code_nodes`, `code_leases`), so a running cluster upgrades
//! in place.
//!
//! Statements are written in SQLite's dialect (code-sql translates them for Postgres), except
//! for the database clock, which differs per backend. Every time compared or stored is the
//! database's clock in ms (`{now}` below), so node clocks never matter for leases: a lease is
//! takeable once its `expires_at` passed on that clock, and the statement that takes it checks
//! that itself.

use super::{Backend, Confirmed, Holder, Member, Registration, Timing};
use crate::{Error, Node, Result, clock};
use code_sql::{Conn, Db, Isolation, OptionalExtension, params};
use parking_lot::Mutex;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS code_nodes (
        id TEXT PRIMARY KEY,
        url TEXT NOT NULL,
        started_at INTEGER NOT NULL,
        heartbeat_at INTEGER NOT NULL,
        draining INTEGER NOT NULL DEFAULT 0
    ) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS code_leases (
        key TEXT PRIMARY KEY,
        node_id TEXT NOT NULL,
        epoch INTEGER NOT NULL,
        expires_at INTEGER NOT NULL
    ) WITHOUT ROWID;
    CREATE INDEX IF NOT EXISTS code_leases_node ON code_leases (node_id, expires_at);
    CREATE INDEX IF NOT EXISTS code_leases_expiry ON code_leases (expires_at, key);
    CREATE INDEX IF NOT EXISTS code_nodes_heartbeat ON code_nodes (heartbeat_at);
";

/// The database's clock, in ms since the epoch, evaluated when the statement runs.
fn now(b: code_sql::Backend) -> &'static str {
    match b {
        code_sql::Backend::Sqlite => "CAST(unixepoch('subsec') * 1000 AS INTEGER)",
        code_sql::Backend::Postgres => "CAST(EXTRACT(EPOCH FROM clock_timestamp()) * 1000 AS BIGINT)",
    }
}

struct Sql {
    now: String,
    heartbeat: String,
    reinsert_node: String,
    live_nodes: String,
    set_draining: String,
    delete_node: String,
    delete_dead_nodes: String,
    renew: String,
    acquire: String,
    release: String,
    delete_lease: String,
    sweep: String,
    lease: String,
}

impl Sql {
    fn new(b: code_sql::Backend) -> Sql {
        let now = now(b);
        Sql {
            now: format!("SELECT {now}"),
            // ?1 id, ?2 started_at (this process's incarnation)
            heartbeat: format!("UPDATE code_nodes SET heartbeat_at = {now} WHERE id = ?1 AND started_at = CAST(?2 AS BIGINT)"),
            // ?1 id, ?2 url, ?3 started_at, ?4 draining, ?5 zone
            reinsert_node: format!(
                "INSERT INTO code_nodes (id, url, started_at, heartbeat_at, draining, zone) VALUES (?1, ?2, CAST(?3 AS BIGINT), {now}, CAST(?4 AS BIGINT), ?5) ON CONFLICT (id) DO NOTHING"
            ),
            // ?1 node_ttl ms
            live_nodes: format!("SELECT id, url, draining, zone FROM code_nodes WHERE heartbeat_at > {now} - CAST(?1 AS BIGINT) ORDER BY id"),
            // ?1 id, ?2 started_at
            set_draining: "UPDATE code_nodes SET draining = 1 WHERE id = ?1 AND started_at = CAST(?2 AS BIGINT)".into(),
            delete_node: "DELETE FROM code_nodes WHERE id = ?1 AND started_at = CAST(?2 AS BIGINT)".into(),
            // ?1 age ms
            delete_dead_nodes: format!("DELETE FROM code_nodes WHERE heartbeat_at < {now} - CAST(?1 AS BIGINT)"),
            // One statement renews every lease this incarnation holds that hasn't expired. A
            // lease that expired is never revived: someone may be about to take it.
            // ?1 lease_ttl ms, ?2 id, ?3 started_at
            renew: format!(
                "UPDATE code_leases SET expires_at = {now} + CAST(?1 AS BIGINT) \
                 WHERE node_id = ?2 AND expires_at > {now} \
                 AND EXISTS (SELECT 1 FROM code_nodes WHERE id = ?2 AND started_at = CAST(?3 AS BIGINT)) \
                 RETURNING key, epoch"
            ),
            // The compare-and-swap: insert, or take over a row that expired (on the database's
            // clock) or is already ours; the epoch goes up when the holder changes. Only this
            // incarnation of the node may take leases (a superseded process with the same pinned
            // id can't). Returns no row if the CAS lost.
            // ?1 key, ?2 id, ?3 lease_ttl ms, ?4 started_at
            acquire: format!(
                "INSERT INTO code_leases (key, node_id, epoch, expires_at) \
                 SELECT CAST(?1 AS TEXT), CAST(?2 AS TEXT), 1, {now} + CAST(?3 AS BIGINT) \
                 WHERE EXISTS (SELECT 1 FROM code_nodes WHERE id = ?2 AND started_at = CAST(?4 AS BIGINT)) \
                 ON CONFLICT (key) DO UPDATE SET \
                   epoch = CASE WHEN code_leases.node_id = excluded.node_id THEN code_leases.epoch ELSE code_leases.epoch + 1 END, \
                   node_id = excluded.node_id, \
                   expires_at = excluded.expires_at \
                 WHERE code_leases.expires_at <= {now} OR code_leases.node_id = excluded.node_id \
                 RETURNING epoch"
            ),
            // Expire a lease now so its preferred node takes it at its next sweep. Only the
            // tenure we held (same node and epoch).
            // ?1 key, ?2 id, ?3 epoch
            release: format!("UPDATE code_leases SET expires_at = {now} - 1 WHERE key = ?1 AND node_id = ?2 AND epoch = CAST(?3 AS BIGINT)"),
            // Give a lease up for good (an idle key, a deleted repo): no sweep takes it back; the
            // next request acquires it again. Only the tenure we held.
            // ?1 key, ?2 id, ?3 epoch
            delete_lease: "DELETE FROM code_leases WHERE key = ?1 AND node_id = ?2 AND epoch = CAST(?3 AS BIGINT)".into(),
            // A page of expired leases after a (expires_at, key) cursor.
            // ?1 cursor expires_at, ?2 cursor key, ?3 page size
            sweep: format!(
                "SELECT key, expires_at FROM code_leases \
                 WHERE expires_at <= {now} AND (expires_at > CAST(?1 AS BIGINT) OR (expires_at = CAST(?1 AS BIGINT) AND key > ?2)) \
                 ORDER BY expires_at, key LIMIT CAST(?3 AS BIGINT)"
            ),
            // A lease with its holder's row. ?1 key
            lease: format!(
                "SELECT l.node_id, l.epoch, l.expires_at, {now}, n.url, n.heartbeat_at, n.draining \
                 FROM code_leases l LEFT JOIN code_nodes n ON n.id = l.node_id WHERE l.key = ?1"
            ),
        }
    }
}

struct Me {
    reg: Registration,
    timing: Timing,
    /// This process's incarnation of the node id (the `started_at` of its row).
    started_at: i64,
}

/// One node's handle on the `code_nodes` and `code_leases` tables of a database.
pub struct Postgres {
    db: Db,
    sql: Sql,
    me: OnceLock<Me>,
    /// Sweep cursor: `(expires_at, key)` of the last row of the previous page.
    cursor: Mutex<(i64, String)>,
}

impl Postgres {
    /// The tables live in `db` (created if needed when the node joins). Give each node a pool of
    /// its own, so renewals never queue behind other work.
    pub fn new(db: Db) -> Postgres {
        let sql = Sql::new(db.backend());
        Postgres { db, sql, me: OnceLock::new(), cursor: Mutex::new((i64::MIN, String::new())) }
    }

    fn me(&self) -> &Me {
        self.me.get().expect("joined")
    }

    fn id(&self) -> &str {
        &self.me().reg.node_id
    }

    fn write<T>(&self, iso: Isolation, mut f: impl FnMut(&Conn<'_>) -> code_sql::Result<T>) -> Result<T> {
        Ok(self.db.write_with(iso, |c| f(c))?)
    }

    fn ms(d: Duration) -> i64 {
        d.as_millis() as i64
    }
}

impl Backend for Postgres {
    fn name(&self) -> &'static str {
        "postgres"
    }

    /// Register this process as `reg.node_id`, waiting (up to `node_ttl + lease_ttl` and a bit)
    /// for a previous process with the same id to look dead and for its leases to expire.
    fn join(&self, reg: &Registration, t: Timing) -> Result<()> {
        self.db.ensure_schema(SCHEMA)?;
        self.db.add_column("code_nodes", "zone", "TEXT")?;
        let deadline = Instant::now() + t.node_ttl + t.lease_ttl + Duration::from_secs(1);
        let node_ttl = Self::ms(t.node_ttl);
        let sql = &self.sql;
        loop {
            let claimed = self.db.write(|c| -> code_sql::Result<Option<i64>> {
                let now: i64 = c.query_row(&sql.now, (), |r| r.get(0))?;
                let prev: Option<(i64, i64)> =
                    c.query_row("SELECT started_at, heartbeat_at FROM code_nodes WHERE id = ?1", [&reg.node_id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
                if prev.is_some_and(|(_, hb)| hb > now - node_ttl) {
                    return Ok(None);
                }
                let held: i64 = c.query_row(
                    "SELECT COUNT(*) FROM code_leases WHERE node_id = ?1 AND expires_at > CAST(?2 AS BIGINT)",
                    params![&reg.node_id, now],
                    |r| r.get(0),
                )?;
                if held > 0 {
                    return Ok(None);
                }
                let started = prev.map_or(now, |(s, _)| now.max(s + 1));
                c.execute(
                    "INSERT INTO code_nodes (id, url, started_at, heartbeat_at, draining, zone) VALUES (?1, ?2, CAST(?3 AS BIGINT), CAST(?4 AS BIGINT), 0, ?5) \
                     ON CONFLICT (id) DO UPDATE SET url = excluded.url, started_at = excluded.started_at, heartbeat_at = excluded.heartbeat_at, draining = 0, zone = excluded.zone",
                    params![&reg.node_id, &reg.url, started, now, reg.zone.as_deref()],
                )?;
                // Expired leases a previous process with this id left behind: disown them, so
                // taking one again counts as a change of holder (the epoch goes up).
                c.execute("UPDATE code_leases SET node_id = '' WHERE node_id = ?1", [&reg.node_id])?;
                Ok(Some(started))
            })?;
            if let Some(started_at) = claimed {
                let _ = self.me.set(Me { reg: reg.clone(), timing: t, started_at });
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(Error::NodeIdInUse(reg.node_id.clone()));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn heartbeat(&self, draining: bool) -> Result<()> {
        let me = self.me();
        let (id, url, started) = (me.reg.node_id.as_str(), me.reg.url.as_str(), me.started_at);
        let superseded = self.write(Isolation::ReadCommitted, |c| {
            if c.execute(&self.sql.heartbeat, params![id, started])? == 1 {
                return Ok(false);
            }
            // No row of ours: either another process took this node id over (it waited for us
            // to look dead first), or our row was cleaned up as dead while we were stalled.
            let other: Option<i64> = c.query_row("SELECT started_at FROM code_nodes WHERE id = ?1", [id], |r| r.get(0)).optional()?;
            if other.is_some() {
                return Ok(true);
            }
            c.execute(&self.sql.reinsert_node, params![id, url, started, draining as i64, me.reg.zone.as_deref()])?;
            Ok(false)
        })?;
        if superseded { Err(Error::Superseded) } else { Ok(()) }
    }

    fn members(&self) -> Result<Vec<Member>> {
        let ttl = Self::ms(self.me().timing.node_ttl);
        Ok(self.db.read(|c| {
            c.query(&self.sql.live_nodes, [ttl], |r| {
                Ok(Member { node: Node { id: r.get(0)?, url: r.get(1)? }, draining: r.get::<_, i64>(2)? != 0, zone: r.get(3)? })
            })
        })?)
    }

    fn set_draining(&self) -> Result<()> {
        self.write(Isolation::ReadCommitted, |c| c.execute(&self.sql.set_draining, params![self.id(), self.me().started_at]))?;
        Ok(())
    }

    fn renew(&self) -> Result<Confirmed> {
        let me = self.me();
        let basis = clock::now();
        let leases = self.write(Isolation::ReadCommitted, |c| {
            c.query(&self.sql.renew, params![Self::ms(me.timing.lease_ttl), self.id(), me.started_at], |r| Ok((r.get(0)?, r.get(1)?)))
        })?;
        Ok(Confirmed { leases, basis })
    }

    fn acquire(&self, keys: &[String]) -> Result<Confirmed> {
        let me = self.me();
        let basis = clock::now();
        let ttl = Self::ms(me.timing.lease_ttl);
        let leases = self.write(Isolation::Serializable, |c| {
            let mut won = Vec::new();
            for k in keys {
                let e = c.query_row(&self.sql.acquire, params![k.as_str(), self.id(), ttl, me.started_at], |r| r.get::<_, i64>(0)).optional()?;
                if let Some(e) = e {
                    won.push((k.clone(), e));
                }
            }
            Ok(won)
        })?;
        Ok(Confirmed { leases, basis })
    }

    fn release(&self, leases: &[(String, i64)]) -> Result<()> {
        self.write(Isolation::ReadCommitted, |c| {
            for (k, e) in leases {
                c.execute(&self.sql.release, params![k, self.id(), *e])?;
            }
            Ok(())
        })
    }

    fn delete(&self, leases: &[(String, i64)]) -> Result<()> {
        self.write(Isolation::ReadCommitted, |c| {
            for (k, e) in leases {
                c.execute(&self.sql.delete_lease, params![k, self.id(), *e])?;
            }
            Ok(())
        })
    }

    fn sweep(&self, page: usize) -> Result<(Vec<String>, bool)> {
        let mut cursor = self.cursor.lock();
        let rows: Vec<(String, i64)> = self.db.read(|c| {
            c.query(&self.sql.sweep, params![cursor.0, cursor.1.as_str(), page as i64], |r| Ok((r.get(0)?, r.get(1)?)))
        })?;
        let full = rows.len() == page;
        *cursor = match (full, rows.last()) {
            (true, Some((k, e))) => (*e, k.clone()),
            _ => (i64::MIN, String::new()),
        };
        Ok((rows.into_iter().map(|(k, _)| k).collect(), full))
    }

    fn lookup(&self, key: &str) -> Result<Holder> {
        let node_ttl = Self::ms(self.me().timing.node_ttl);
        let row = self.db.read(|c| {
            c.query_row(&self.sql.lease, [key], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?, r.get::<_, Option<String>>(4)?, r.get::<_, Option<i64>>(5)?))
            })
            .optional()
        })?;
        let Some((node, expires_at, now, url, heartbeat_at)) = row.filter(|r| r.1 > r.2) else { return Ok(Holder::Free) };
        let left = Duration::from_millis((expires_at - now).max(1) as u64);
        if node == self.id() {
            return Ok(Holder::Me { left });
        }
        let live = heartbeat_at.is_some_and(|hb| hb > now - node_ttl);
        Ok(Holder::Other { id: node, url, live, left })
    }

    fn leave(&self, _released_all: bool) -> Result<()> {
        self.write(Isolation::ReadCommitted, |c| c.execute(&self.sql.delete_node, params![self.id(), self.me().started_at]))?;
        Ok(())
    }

    fn cleanup(&self) -> Result<()> {
        let age = Self::ms((self.me().timing.node_ttl * 12).max(Duration::from_secs(60)));
        self.write(Isolation::ReadCommitted, |c| c.execute(&self.sql.delete_dead_nodes, [age]))?;
        Ok(())
    }

    fn holder(&self, key: &str) -> Result<Option<(String, i64)>> {
        Ok(self.db.read(|c| c.query_row("SELECT node_id, epoch FROM code_leases WHERE key = ?1", [key], |r| Ok((r.get(0)?, r.get(1)?))).optional())?)
    }
}

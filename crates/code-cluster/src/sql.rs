//! The tables and every statement this crate runs. Written in SQLite's dialect (code-sql
//! translates them for Postgres), except for the database clock, which differs per backend.
//!
//! Every time compared or stored is the database's clock in ms (`{now}` below), so node clocks
//! never matter for leases.

use code_sql::Backend;

pub(crate) const SCHEMA: &str = "
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
fn now(b: Backend) -> &'static str {
    match b {
        Backend::Sqlite => "CAST(unixepoch('subsec') * 1000 AS INTEGER)",
        Backend::Postgres => "CAST(EXTRACT(EPOCH FROM clock_timestamp()) * 1000 AS BIGINT)",
    }
}

pub(crate) struct Sql {
    pub now: String,
    pub heartbeat: String,
    pub reinsert_node: String,
    pub live_nodes: String,
    pub set_draining: String,
    pub delete_node: String,
    pub delete_dead_nodes: String,
    pub renew: String,
    pub acquire: String,
    pub release: String,
    pub sweep: String,
    pub lease: String,
}

impl Sql {
    pub(crate) fn new(b: Backend) -> Sql {
        let now = now(b);
        Sql {
            now: format!("SELECT {now}"),
            // ?1 id, ?2 started_at (this process's incarnation)
            heartbeat: format!("UPDATE code_nodes SET heartbeat_at = {now} WHERE id = ?1 AND started_at = CAST(?2 AS BIGINT)"),
            // ?1 id, ?2 url, ?3 started_at, ?4 draining
            reinsert_node: format!(
                "INSERT INTO code_nodes (id, url, started_at, heartbeat_at, draining) VALUES (?1, ?2, CAST(?3 AS BIGINT), {now}, CAST(?4 AS BIGINT)) ON CONFLICT (id) DO NOTHING"
            ),
            // ?1 node_ttl ms
            live_nodes: format!("SELECT id, url, draining FROM code_nodes WHERE heartbeat_at > {now} - CAST(?1 AS BIGINT) ORDER BY id"),
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

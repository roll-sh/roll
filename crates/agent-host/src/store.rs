//! The daemon's entity store: `agent.db`, SQLite in WAL mode (docs/code/agents.md, "Sequences,
//! sync and resume").
//!
//! Every record is kept once, at its newest `seq` (the key is `(kind, id)`), so "everything after
//! `seq` N" is a range scan on the `seq` index and a replay never repeats a record. Deleting a
//! record writes a `Removed` tombstone under the same key, which replays like any other upsert.
//! Command receipts (T3's `CommandReceiptStore`) make command ids idempotent across restarts, and
//! `thread_state` holds the orchestrator's private per-thread state (native conversation ids,
//! pending handoffs) that is never mirrored.
//!
//! A write commits before its `on_commit` callback runs, and the callback runs under the store's
//! lock: the orchestrator broadcasts there, so subscribers see upserts in `seq` order and never
//! see one that isn't durable.

use agent_proto::{Entity, EntityKind, Reply, Stamped, ThreadId};
use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::path::Path;
use std::sync::Mutex;

/// Migrations, applied in order; `PRAGMA user_version` is how many have run.
const MIGRATIONS: &[&str] = &[
    "CREATE TABLE entities (
         kind TEXT NOT NULL,
         id TEXT NOT NULL,
         thread TEXT,
         removed INTEGER NOT NULL DEFAULT 0,
         seq INTEGER NOT NULL,
         json TEXT NOT NULL,
         PRIMARY KEY (kind, id)
     );
     CREATE UNIQUE INDEX entities_seq ON entities (seq);
     CREATE INDEX entities_thread ON entities (thread, kind);
     CREATE TABLE counters (name TEXT PRIMARY KEY, value INTEGER NOT NULL);
     INSERT INTO counters (name, value) VALUES ('seq', 0);
     CREATE TABLE receipts (id TEXT PRIMARY KEY, reply TEXT NOT NULL, at INTEGER NOT NULL);
     CREATE TABLE thread_state (thread TEXT PRIMARY KEY, json TEXT NOT NULL);",
];

pub struct Store {
    db: Mutex<Connection>,
}

pub fn kind_name(kind: EntityKind) -> &'static str {
    match kind {
        EntityKind::Project => "project",
        EntityKind::Thread => "thread",
        EntityKind::Run => "run",
        EntityKind::Node => "node",
        EntityKind::Request => "request",
        EntityKind::Checkpoint => "checkpoint",
        EntityKind::Terminal => "terminal",
    }
}

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Store> {
        let mut db = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "synchronous", "NORMAL")?;
        db.pragma_update(None, "foreign_keys", "ON")?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        migrate(&mut db)?;
        Ok(Store { db: Mutex::new(db) })
    }

    fn db(&self) -> std::sync::MutexGuard<'_, Connection> {
        // A panic while holding the lock can't leave SQLite half-written (transactions roll
        // back on drop), so a poisoned lock is still usable.
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Writes `entities` in one transaction, each with the next `seq`, then calls `on_commit`
    /// with the stamped records while still holding the lock.
    pub fn put(&self, entities: Vec<Entity>, on_commit: impl FnOnce(&[Stamped])) -> anyhow::Result<Vec<Stamped>> {
        if entities.is_empty() {
            return Ok(Vec::new());
        }
        let mut db = self.db();
        let tx = db.transaction()?;
        let mut seq = tx.query_row("SELECT value FROM counters WHERE name = 'seq'", [], |r| r.get::<_, i64>(0))? as u64;
        let mut stamped = Vec::with_capacity(entities.len());
        {
            let mut upsert = tx.prepare_cached(
                "INSERT INTO entities (kind, id, thread, removed, seq, json) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (kind, id) DO UPDATE SET
                     thread = excluded.thread, removed = excluded.removed, seq = excluded.seq, json = excluded.json",
            )?;
            for entity in entities {
                seq += 1;
                let json = serde_json::to_string(&entity)?;
                upsert.execute(params![
                    kind_name(entity.kind()),
                    entity.id(),
                    entity.thread().map(|t| t.as_str()),
                    matches!(entity, Entity::Removed(_)),
                    seq as i64,
                    json,
                ])?;
                stamped.push(Stamped { seq, entity });
            }
        }
        tx.execute("UPDATE counters SET value = ?1 WHERE name = 'seq'", [seq as i64])?;
        tx.commit()?;
        on_commit(&stamped);
        Ok(stamped)
    }

    /// The newest `seq` handed out.
    pub fn last_seq(&self) -> anyhow::Result<u64> {
        Ok(self.db().query_row("SELECT value FROM counters WHERE name = 'seq'", [], |r| r.get::<_, i64>(0))? as u64)
    }

    /// Records (tombstones included) with `seq > after`, oldest first.
    pub fn since(&self, after: u64, limit: usize) -> anyhow::Result<Vec<Stamped>> {
        let db = self.db();
        let mut stmt = db.prepare_cached("SELECT seq, json FROM entities WHERE seq > ?1 ORDER BY seq LIMIT ?2")?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = stmt.query_map(params![after as i64, limit], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (seq, json) = row?;
            out.push(Stamped { seq: seq as u64, entity: serde_json::from_str(&json)? });
        }
        Ok(out)
    }

    /// A live record (not a tombstone).
    pub fn get(&self, kind: EntityKind, id: &str) -> anyhow::Result<Option<Entity>> {
        let db = self.db();
        let json: Option<String> = db
            .query_row(
                "SELECT json FROM entities WHERE kind = ?1 AND id = ?2 AND removed = 0",
                params![kind_name(kind), id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(json.map(|j| serde_json::from_str(&j)).transpose()?)
    }

    /// Every live record of a kind, oldest change first.
    pub fn list(&self, kind: EntityKind) -> anyhow::Result<Vec<Entity>> {
        self.query("SELECT json FROM entities WHERE kind = ?1 AND removed = 0 ORDER BY seq", params![kind_name(kind)])
    }

    /// A thread's live records of a kind, oldest change first.
    pub fn of_thread(&self, thread: &ThreadId, kind: EntityKind) -> anyhow::Result<Vec<Entity>> {
        self.query(
            "SELECT json FROM entities WHERE thread = ?1 AND kind = ?2 AND removed = 0 ORDER BY seq",
            params![thread.as_str(), kind_name(kind)],
        )
    }

    fn query(&self, sql: &str, params: impl rusqlite::Params) -> anyhow::Result<Vec<Entity>> {
        let db = self.db();
        let mut stmt = db.prepare_cached(sql)?;
        let rows = stmt.query_map(params, |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str(&row?)?);
        }
        Ok(out)
    }

    pub fn receipt(&self, id: &str) -> anyhow::Result<Option<Reply>> {
        let db = self.db();
        let json: Option<String> =
            db.query_row("SELECT reply FROM receipts WHERE id = ?1", [id], |r| r.get(0)).optional()?;
        Ok(json.map(|j| serde_json::from_str(&j)).transpose()?)
    }

    pub fn put_receipt(&self, id: &str, reply: &Reply, at: u64) -> anyhow::Result<()> {
        self.db().execute(
            "INSERT INTO receipts (id, reply, at) VALUES (?1, ?2, ?3) ON CONFLICT (id) DO NOTHING",
            params![id, serde_json::to_string(reply)?, at as i64],
        )?;
        Ok(())
    }

    /// Forgets receipts older than `before`; returns how many.
    pub fn prune_receipts(&self, before: u64) -> anyhow::Result<usize> {
        Ok(self.db().execute("DELETE FROM receipts WHERE at < ?1", [before as i64])?)
    }

    pub fn thread_state<T: DeserializeOwned>(&self, thread: &ThreadId) -> anyhow::Result<Option<T>> {
        let db = self.db();
        let json: Option<String> = db
            .query_row("SELECT json FROM thread_state WHERE thread = ?1", [thread.as_str()], |r| r.get(0))
            .optional()?;
        Ok(json.map(|j| serde_json::from_str(&j)).transpose()?)
    }

    pub fn put_thread_state<T: Serialize>(&self, thread: &ThreadId, state: &T) -> anyhow::Result<()> {
        self.db().execute(
            "INSERT INTO thread_state (thread, json) VALUES (?1, ?2) ON CONFLICT (thread) DO UPDATE SET json = excluded.json",
            params![thread.as_str(), serde_json::to_string(state)?],
        )?;
        Ok(())
    }

    pub fn delete_thread_state(&self, thread: &ThreadId) -> anyhow::Result<()> {
        self.db().execute("DELETE FROM thread_state WHERE thread = ?1", [thread.as_str()])?;
        Ok(())
    }
}

fn migrate(db: &mut Connection) -> anyhow::Result<()> {
    let version = db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))? as usize;
    if version > MIGRATIONS.len() {
        anyhow::bail!("agent.db is from a newer roll (schema {version}, this one knows {})", MIGRATIONS.len());
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(version) {
        let tx = db.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", (i + 1) as i64)?;
        tx.commit()?;
    }
    Ok(())
}

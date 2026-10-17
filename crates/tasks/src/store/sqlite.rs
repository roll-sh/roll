//! SQLite store for a single host (and tests). WAL mode, one connection behind a mutex; the
//! records are JSON with the indexed fields copied into columns.
//!
//! It is also the engine under the log store ([`super::log`]): it can record every change (a
//! SQLite session), cut them into batches, replay batches, and copy itself to a snapshot file
//! a few pages at a time while it keeps serving.

use super::*;
use rusqlite::backup::{Backup, StepResult};
use rusqlite::session::{ConflictAction, ConflictType, Session};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::sync::Mutex;

pub struct SqliteStore {
    db: Mutex<Inner>,
    path: String,
    shards: u32,
}

/// The connection plus the change recorder and snapshot copy that borrow it. Fields drop in
/// order, so the borrowers go before what they point into.
struct Inner {
    backup: Option<Backup<'static, 'static>>,
    session: Option<Session<'static>>,
    snapshot: Option<Box<Connection>>,
    conn: Box<Connection>,
}

// The session and backup are only touched with the mutex held, on whichever thread holds it;
// SQLite is built thread-safe.
unsafe impl Send for Inner {}

impl std::ops::Deref for Inner {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.conn
    }
}

impl Inner {
    fn new(conn: Connection) -> Self {
        Self { backup: None, session: None, snapshot: None, conn: Box::new(conn) }
    }

    /// The connection with a lifetime the borrowers can hold; it lives (boxed, unmoved) as
    /// long as `self`, and they are dropped first.
    fn conn_static(&self) -> &'static Connection {
        unsafe { &*(self.conn.as_ref() as *const Connection) }
    }

    fn new_session(&self) -> Result<Session<'static>> {
        let mut s = Session::new(self.conn_static())?;
        s.attach(None::<&str>)?;
        Ok(s)
    }

    /// Changes since the last cut (a patchset: new values only, about half a changeset), and
    /// start recording afresh.
    fn cut(&mut self) -> Result<Vec<u8>> {
        let Some(s) = self.session.as_mut() else { return Ok(Vec::new()) };
        if s.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        s.patchset_strm(&mut out)?;
        self.session = None;
        self.session = Some(self.new_session()?);
        Ok(out)
    }
}

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA busy_timeout = 5000;

CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY,
    lane TEXT NOT NULL,
    state TEXT NOT NULL,
    due_at INTEGER NOT NULL,
    kind TEXT NOT NULL,
    queue TEXT,
    topic TEXT,
    schedule_id TEXT,
    run_id TEXT,
    label TEXT,
    url TEXT,
    group_key TEXT,
    expires_at INTEGER,
    version INTEGER NOT NULL,
    data TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS messages_due ON messages (lane, due_at) WHERE state IN ('pending', 'active');
CREATE INDEX IF NOT EXISTS messages_group ON messages (group_key, id) WHERE group_key IS NOT NULL AND state IN ('pending', 'active');
CREATE INDEX IF NOT EXISTS messages_state ON messages (state, id);
CREATE INDEX IF NOT EXISTS messages_queue ON messages (queue, id) WHERE queue IS NOT NULL;
CREATE INDEX IF NOT EXISTS messages_run ON messages (run_id) WHERE run_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS messages_expiry ON messages (expires_at) WHERE expires_at IS NOT NULL;

CREATE TABLE IF NOT EXISTS bodies (id TEXT PRIMARY KEY, body BLOB NOT NULL);

CREATE TABLE IF NOT EXISTS attempts (
    message_id TEXT NOT NULL,
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    data TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS attempts_message ON attempts (message_id, seq);

CREATE TABLE IF NOT EXISTS kv (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    version INTEGER NOT NULL,
    expires_at INTEGER
);

CREATE TABLE IF NOT EXISTS docs (
    kind TEXT NOT NULL,
    id TEXT NOT NULL,
    value TEXT NOT NULL,
    version INTEGER NOT NULL,
    PRIMARY KEY (kind, id)
);

CREATE TABLE IF NOT EXISTS runs (
    id TEXT PRIMARY KEY,
    function_id TEXT NOT NULL,
    status TEXT NOT NULL,
    expires_at INTEGER,
    version INTEGER NOT NULL,
    data TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS runs_function ON runs (function_id, id);
CREATE INDEX IF NOT EXISTS runs_status ON runs (status, id);

CREATE TABLE IF NOT EXISTS steps (
    run_id TEXT NOT NULL,
    id TEXT NOT NULL,
    data TEXT NOT NULL,
    PRIMARY KEY (run_id, id)
);

CREATE TABLE IF NOT EXISTS events (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    expires_at INTEGER,
    data TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS events_name ON events (name, id);

CREATE TABLE IF NOT EXISTS waits (
    event TEXT NOT NULL,
    key TEXT NOT NULL,
    id TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    data TEXT NOT NULL,
    PRIMARY KEY (event, key, id)
);

CREATE TABLE IF NOT EXISTS usage (
    day INTEGER NOT NULL,
    field TEXT NOT NULL,
    n INTEGER NOT NULL,
    PRIMARY KEY (day, field)
);
"#;

impl SqliteStore {
    pub fn open(path: &Path, shards: u32) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let db = Connection::open(path)?;
        db.execute_batch(SCHEMA)?;
        Ok(Self { db: Mutex::new(Inner::new(db)), path: path.display().to_string(), shards: shards.max(1) })
    }

    pub fn memory(shards: u32) -> Result<Self> {
        let db = Connection::open_in_memory()?;
        db.execute_batch(SCHEMA)?;
        Ok(Self { db: Mutex::new(Inner::new(db)), path: ":memory:".into(), shards: shards.max(1) })
    }

    /// A working copy for the log store: durability comes from the log, so the file skips
    /// fsyncs (it is rebuilt from the log after any crash).
    pub fn open_unsynced(path: &Path, shards: u32) -> Result<Self> {
        let s = Self::open(path, shards)?;
        s.db().execute_batch("PRAGMA synchronous = OFF;")?;
        Ok(s)
    }

    fn db(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Start recording changes for [`Self::cut`].
    pub fn record_changes(&self) -> Result<()> {
        let mut db = self.db();
        if db.session.is_none() {
            db.session = Some(db.new_session()?);
        }
        Ok(())
    }

    /// Whether anything changed since the last cut.
    pub fn has_changes(&self) -> bool {
        self.db().session.as_ref().is_some_and(|s| !s.is_empty())
    }

    /// Cap the page cache (many small databases share a node).
    pub fn set_cache_kib(&self, kib: i64) -> Result<()> {
        self.db().execute_batch(&format!("PRAGMA cache_size = -{};", kib.max(64)))?;
        Ok(())
    }

    /// The net changes since the last cut, as a SQLite patchset (empty when nothing changed).
    pub fn cut(&self) -> Result<Vec<u8>> {
        self.db().cut()
    }

    /// Replay a patchset from [`Self::cut`]. Replays are idempotent: a row already in its
    /// final state is left as it is.
    pub fn apply_changes(&self, changes: &[u8]) -> Result<()> {
        if changes.is_empty() {
            return Ok(());
        }
        let db = self.db();
        let mut input: &[u8] = changes;
        db.apply_strm(&mut input, None::<fn(&str) -> bool>, |kind, _| match kind {
            ConflictType::SQLITE_CHANGESET_DATA | ConflictType::SQLITE_CHANGESET_CONFLICT => ConflictAction::SQLITE_CHANGESET_REPLACE,
            _ => ConflictAction::SQLITE_CHANGESET_OMIT,
        })?;
        Ok(())
    }

    /// Begin copying the database to `dst` (see [`Self::snapshot_step`]).
    pub fn snapshot_begin(&self, dst: &Path) -> Result<()> {
        let mut db = self.db();
        db.backup = None;
        db.snapshot = None;
        let _ = std::fs::remove_file(dst);
        let mut target = Box::new(Connection::open(dst)?);
        let target_ref: &'static mut Connection = unsafe { &mut *(target.as_mut() as *mut Connection) };
        let backup = Backup::new(db.conn_static(), target_ref)?;
        db.snapshot = Some(target);
        db.backup = Some(backup);
        Ok(())
    }

    /// Copy up to `pages` more pages. Writes made meanwhile reach the copy too, so when the
    /// copy completes it is exactly the current state; at that instant the recorded changes
    /// are cut and returned: the snapshot is the state after them.
    pub fn snapshot_step(&self, pages: i32) -> Result<Option<Vec<u8>>> {
        let mut db = self.db();
        let Some(backup) = db.backup.as_ref() else { anyhow::bail!("no snapshot in progress") };
        match backup.step(pages)? {
            StepResult::Done => {
                db.backup = None;
                db.snapshot = None;
                Ok(Some(db.cut()?))
            }
            StepResult::More | StepResult::Busy | StepResult::Locked => Ok(None),
            _ => Ok(None),
        }
    }

    pub fn snapshot_abort(&self) {
        let mut db = self.db();
        db.backup = None;
        db.snapshot = None;
    }

    /// Database size in bytes.
    pub fn size_bytes(&self) -> u64 {
        let db = self.db();
        let pages: i64 = db.query_row("PRAGMA page_count", [], |r| r.get(0)).unwrap_or(0);
        let size: i64 = db.query_row("PRAGMA page_size", [], |r| r.get(0)).unwrap_or(4096);
        (pages * size) as u64
    }
}


fn msg_row(row: &rusqlite::Row) -> rusqlite::Result<Message> {
    let data: String = row.get("data")?;
    let mut m: Message = serde_json::from_str(&data)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
    m.version = row.get::<_, i64>("version")? as u64;
    Ok(m)
}

fn json_row<T: DeserializeOwned>(row: &rusqlite::Row, col: &str) -> rusqlite::Result<T> {
    let data: String = row.get(col)?;
    serde_json::from_str(&data)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))
}

fn expect_clause(expect: Expect) -> (&'static str, i64) {
    match expect {
        Expect::Any => ("1 = 1 OR ?1 = ?1", 0),
        Expect::Absent => ("0 = 1 AND ?1 = ?1", 0),
        Expect::Version(v) => ("version = ?1", v as i64),
    }
}

#[async_trait]
impl Store for SqliteStore {
    fn describe(&self) -> String {
        format!("sqlite {}", self.path)
    }

    fn shards(&self) -> u32 {
        self.shards
    }

    async fn msg_insert(&self, m: &Message, body: &[u8]) -> Result<()> {
        let db = self.db();
        let tx = db.unchecked_transaction()?;
        let stored = Message { version: 1, ..m.clone() };
        tx.execute(
            "INSERT INTO messages (id, lane, state, due_at, kind, queue, topic, schedule_id, run_id, label, url,
                                   group_key, expires_at, version, data)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 1, ?14)",
            params![
                m.id,
                m.lane,
                m.state.as_str(),
                m.due_at,
                m.target.kind(),
                m.queue,
                m.topic,
                m.schedule_id,
                m.run_id,
                m.label,
                m.target.url(),
                m.group,
                m.expires_at,
                serde_json::to_string(&stored)?
            ],
        )?;
        tx.execute("INSERT INTO bodies (id, body) VALUES (?1, ?2)", params![m.id, body])?;
        tx.commit()?;
        Ok(())
    }

    async fn msg_get(&self, id: &str) -> Result<Option<Message>> {
        Ok(self.db().query_row("SELECT data, version FROM messages WHERE id = ?1", [id], msg_row).optional()?)
    }

    async fn msg_body(&self, id: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.db().query_row("SELECT body FROM bodies WHERE id = ?1", [id], |r| r.get(0)).optional()?)
    }

    async fn msg_update(&self, m: &Message, expect: u64) -> Result<bool> {
        let stored = Message { version: expect + 1, ..m.clone() };
        let n = self.db().execute(
            "UPDATE messages SET lane = ?2, state = ?3, due_at = ?4, label = ?5, expires_at = ?6, version = ?7, data = ?8
             WHERE id = ?1 AND version = ?9",
            params![
                m.id,
                m.lane,
                m.state.as_str(),
                m.due_at,
                m.label,
                m.expires_at,
                (expect + 1) as i64,
                serde_json::to_string(&stored)?,
                expect as i64
            ],
        )?;
        Ok(n == 1)
    }

    async fn msg_delete(&self, id: &str) -> Result<()> {
        let db = self.db();
        db.execute("DELETE FROM messages WHERE id = ?1", [id])?;
        db.execute("DELETE FROM bodies WHERE id = ?1", [id])?;
        db.execute("DELETE FROM attempts WHERE message_id = ?1", [id])?;
        Ok(())
    }

    async fn msg_due(&self, lane: &str, now: Ms, limit: usize) -> Result<Due> {
        let db = self.db();
        let mut stmt = db.prepare_cached(
            "SELECT id, due_at FROM messages WHERE lane = ?1 AND state IN ('pending', 'active')
             ORDER BY due_at, id LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![lane, (limit + 1) as i64], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        let mut due = Due::default();
        for row in rows {
            let (id, at) = row?;
            if at <= now && due.ids.len() < limit {
                due.ids.push(id);
            } else if at > now {
                due.next_at = Some(at);
                break;
            }
        }
        Ok(due)
    }

    async fn msg_group_head(&self, group: &str) -> Result<Option<String>> {
        Ok(self
            .db()
            .query_row(
                "SELECT id FROM messages WHERE group_key = ?1 AND state IN ('pending', 'active') ORDER BY id LIMIT 1",
                [group],
                |r| r.get(0),
            )
            .optional()?)
    }

    async fn msg_list(&self, q: &MessageQuery) -> Result<Page<Message>> {
        let mut sql = String::from("SELECT data, version FROM messages WHERE 1 = 1");
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        let mut arg = |sql: &mut String, clause: &str, v: Box<dyn rusqlite::ToSql>| {
            args.push(v);
            sql.push_str(&clause.replace('?', &format!("?{}", args.len())));
        };
        match q.state {
            Some(MsgState::Pending | MsgState::Active) => sql.push_str(" AND state IN ('pending', 'active')"),
            Some(s) => arg(&mut sql, " AND state = ?", Box::new(s.as_str())),
            None => {}
        }
        if let Some(v) = &q.queue {
            arg(&mut sql, " AND queue = ?", Box::new(v.clone()));
        }
        if let Some(v) = &q.topic {
            arg(&mut sql, " AND topic = ?", Box::new(v.clone()));
        }
        if let Some(v) = &q.schedule_id {
            arg(&mut sql, " AND schedule_id = ?", Box::new(v.clone()));
        }
        if let Some(v) = &q.run_id {
            arg(&mut sql, " AND run_id = ?", Box::new(v.clone()));
        }
        if let Some(v) = &q.label {
            arg(&mut sql, " AND label = ?", Box::new(v.clone()));
        }
        if let Some(v) = &q.url {
            arg(&mut sql, " AND url = ?", Box::new(v.clone()));
        }
        if q.user_only {
            sql.push_str(" AND kind IN ('http', 'pull')");
        }
        if let Some(c) = &q.cursor {
            arg(&mut sql, " AND id < ?", Box::new(c.clone()));
        }
        let limit = q.limit.clamp(1, 1000);
        sql.push_str(&format!(" ORDER BY id DESC LIMIT {}", limit + 1));
        let db = self.db();
        let mut stmt = db.prepare(&sql)?;
        let mut items =
            stmt.query_map(rusqlite::params_from_iter(args.iter().map(|a| a.as_ref())), msg_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        let cursor = if items.len() > limit {
            items.truncate(limit);
            items.last().map(|m| m.id.clone())
        } else {
            None
        };
        Ok(Page { items, cursor })
    }

    async fn lane_depth(&self, lane: &str, now: Ms) -> Result<(u64, u64, u64)> {
        Ok(self.db().query_row(
            "SELECT
                COALESCE(SUM(state = 'pending' AND due_at <= ?2), 0),
                COALESCE(SUM(state = 'pending' AND due_at > ?2), 0),
                COALESCE(SUM(state = 'active'), 0)
             FROM messages WHERE lane = ?1 AND state IN ('pending', 'active')",
            params![lane, now],
            |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64, r.get::<_, i64>(2)? as u64)),
        )?)
    }

    async fn attempt_add(&self, a: &Attempt) -> Result<()> {
        self.db().execute(
            "INSERT INTO attempts (message_id, data) VALUES (?1, ?2)",
            params![a.message_id, serde_json::to_string(a)?],
        )?;
        Ok(())
    }

    async fn attempts(&self, message_id: &str) -> Result<Vec<Attempt>> {
        let db = self.db();
        let mut stmt = db.prepare_cached("SELECT data FROM attempts WHERE message_id = ?1 ORDER BY seq")?;
        Ok(stmt.query_map([message_id], |r| json_row(r, "data"))?.collect::<rusqlite::Result<_>>()?)
    }

    async fn kv_get(&self, key: &str) -> Result<Option<Versioned<Value>>> {
        let now = now_ms();
        Ok(self
            .db()
            .query_row(
                "SELECT value, version FROM kv WHERE key = ?1 AND (expires_at IS NULL OR expires_at > ?2)",
                params![key, now],
                |r| Ok(Versioned { value: json_row(r, "value")?, version: r.get::<_, i64>(1)? as u64 }),
            )
            .optional()?)
    }

    async fn kv_put(&self, key: &str, value: &Value, expect: Expect, expires_at: Option<Ms>) -> Result<bool> {
        let now = now_ms();
        let db = self.db();
        let current: Option<(i64, Option<i64>)> = db
            .query_row("SELECT version, expires_at FROM kv WHERE key = ?1", [key], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        let live = current.filter(|(_, exp)| exp.is_none_or(|e| e > now));
        let next = match (expect, live) {
            (Expect::Absent, Some(_)) => return Ok(false),
            (Expect::Version(v), Some((cur, _))) if cur as u64 != v => return Ok(false),
            (Expect::Version(_), None) => return Ok(false),
            (_, Some((cur, _))) => cur + 1,
            // An expired record is replaced; versions keep counting so stale CAS can't win.
            (_, None) => current.map(|(v, _)| v + 1).unwrap_or(1),
        };
        db.execute(
            "INSERT INTO kv (key, value, version, expires_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value, version = excluded.version, expires_at = excluded.expires_at",
            params![key, serde_json::to_string(value)?, next, expires_at],
        )?;
        Ok(true)
    }

    async fn kv_delete(&self, key: &str, expect: Expect) -> Result<bool> {
        let (clause, v) = expect_clause(expect);
        let n = self.db().execute(&format!("DELETE FROM kv WHERE key = ?2 AND ({clause})"), params![v, key])?;
        Ok(n == 1)
    }

    async fn doc_get(&self, kind: &str, id: &str) -> Result<Option<Versioned<Value>>> {
        Ok(self
            .db()
            .query_row("SELECT value, version FROM docs WHERE kind = ?1 AND id = ?2", params![kind, id], |r| {
                Ok(Versioned { value: json_row(r, "value")?, version: r.get::<_, i64>(1)? as u64 })
            })
            .optional()?)
    }

    async fn doc_put(&self, kind: &str, id: &str, value: &Value, expect: Expect) -> Result<bool> {
        let db = self.db();
        let body = serde_json::to_string(value)?;
        let n = match expect {
            Expect::Any => db.execute(
                "INSERT INTO docs (kind, id, value, version) VALUES (?1, ?2, ?3, 1)
                 ON CONFLICT (kind, id) DO UPDATE SET value = excluded.value, version = docs.version + 1",
                params![kind, id, body],
            )?,
            Expect::Absent => db.execute(
                "INSERT INTO docs (kind, id, value, version) VALUES (?1, ?2, ?3, 1) ON CONFLICT DO NOTHING",
                params![kind, id, body],
            )?,
            Expect::Version(v) => db.execute(
                "UPDATE docs SET value = ?3, version = version + 1 WHERE kind = ?1 AND id = ?2 AND version = ?4",
                params![kind, id, body, v as i64],
            )?,
        };
        Ok(n == 1)
    }

    async fn doc_delete(&self, kind: &str, id: &str) -> Result<bool> {
        Ok(self.db().execute("DELETE FROM docs WHERE kind = ?1 AND id = ?2", params![kind, id])? == 1)
    }

    async fn doc_list(&self, kind: &str) -> Result<Vec<Versioned<Value>>> {
        let db = self.db();
        let mut stmt = db.prepare_cached("SELECT value, version FROM docs WHERE kind = ?1 ORDER BY id")?;
        Ok(stmt
            .query_map([kind], |r| Ok(Versioned { value: json_row(r, "value")?, version: r.get::<_, i64>(1)? as u64 }))?
            .collect::<rusqlite::Result<_>>()?)
    }

    async fn run_get(&self, id: &str) -> Result<Option<Run>> {
        Ok(self
            .db()
            .query_row("SELECT data, version FROM runs WHERE id = ?1", [id], |r| {
                let mut run: Run = json_row(r, "data")?;
                run.version = r.get::<_, i64>(1)? as u64;
                Ok(run)
            })
            .optional()?)
    }

    async fn run_put(&self, run: &Run, expect: Expect) -> Result<bool> {
        let db = self.db();
        let n = match expect {
            Expect::Absent => {
                let stored = Run { version: 1, ..run.clone() };
                db.execute(
                    "INSERT INTO runs (id, function_id, status, expires_at, version, data) VALUES (?1, ?2, ?3, ?4, 1, ?5)
                     ON CONFLICT DO NOTHING",
                    params![run.id, run.function_id, run.status.as_str(), run.expires_at, serde_json::to_string(&stored)?],
                )?
            }
            Expect::Version(v) => {
                let stored = Run { version: v + 1, ..run.clone() };
                db.execute(
                    "UPDATE runs SET status = ?2, expires_at = ?3, version = ?4, data = ?5 WHERE id = ?1 AND version = ?6",
                    params![run.id, run.status.as_str(), run.expires_at, (v + 1) as i64, serde_json::to_string(&stored)?, v as i64],
                )?
            }
            Expect::Any => {
                let stored = Run { version: run.version + 1, ..run.clone() };
                db.execute(
                    "INSERT INTO runs (id, function_id, status, expires_at, version, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT (id) DO UPDATE SET status = excluded.status, expires_at = excluded.expires_at,
                         version = excluded.version, data = excluded.data",
                    params![
                        run.id,
                        run.function_id,
                        run.status.as_str(),
                        run.expires_at,
                        stored.version as i64,
                        serde_json::to_string(&stored)?
                    ],
                )?
            }
        };
        Ok(n == 1)
    }

    async fn run_list(&self, q: &RunQuery) -> Result<Page<Run>> {
        let mut sql = String::from("SELECT data, version FROM runs WHERE 1 = 1");
        let mut args: Vec<String> = Vec::new();
        if let Some(f) = &q.function_id {
            args.push(f.clone());
            sql.push_str(&format!(" AND function_id = ?{}", args.len()));
        }
        if let Some(s) = q.status {
            args.push(s.as_str().into());
            sql.push_str(&format!(" AND status = ?{}", args.len()));
        }
        if let Some(c) = &q.cursor {
            args.push(c.clone());
            sql.push_str(&format!(" AND id < ?{}", args.len()));
        }
        let limit = q.limit.clamp(1, 1000);
        sql.push_str(&format!(" ORDER BY id DESC LIMIT {}", limit + 1));
        let db = self.db();
        let mut stmt = db.prepare(&sql)?;
        let mut items = stmt
            .query_map(rusqlite::params_from_iter(args.iter()), |r| {
                let mut run: Run = json_row(r, "data")?;
                run.version = r.get::<_, i64>(1)? as u64;
                Ok(run)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let cursor = if items.len() > limit {
            items.truncate(limit);
            items.last().map(|r| r.id.clone())
        } else {
            None
        };
        Ok(Page { items, cursor })
    }

    async fn steps(&self, run_id: &str) -> Result<Vec<Step>> {
        let db = self.db();
        let mut stmt = db.prepare_cached("SELECT data FROM steps WHERE run_id = ?1")?;
        let mut steps: Vec<Step> = stmt.query_map([run_id], |r| json_row(r, "data"))?.collect::<rusqlite::Result<_>>()?;
        steps.sort_by_key(|s| s.at);
        Ok(steps)
    }

    async fn step_put(&self, step: &Step) -> Result<()> {
        self.db().execute(
            "INSERT INTO steps (run_id, id, data) VALUES (?1, ?2, ?3)
             ON CONFLICT (run_id, id) DO UPDATE SET data = excluded.data",
            params![step.run_id, step.id, serde_json::to_string(step)?],
        )?;
        Ok(())
    }

    async fn event_put(&self, e: &Event) -> Result<()> {
        self.db().execute(
            "INSERT INTO events (id, name, expires_at, data) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (id) DO UPDATE SET data = excluded.data",
            params![e.id, e.name, e.expires_at, serde_json::to_string(e)?],
        )?;
        Ok(())
    }

    async fn event_get(&self, id: &str) -> Result<Option<Event>> {
        Ok(self.db().query_row("SELECT data FROM events WHERE id = ?1", [id], |r| json_row(r, "data")).optional()?)
    }

    async fn event_list(&self, q: &EventQuery) -> Result<Page<Event>> {
        let limit = q.limit.clamp(1, 1000);
        let db = self.db();
        let mut stmt = db.prepare(&format!(
            "SELECT data FROM events WHERE (?1 IS NULL OR name = ?1) AND (?2 IS NULL OR id < ?2) ORDER BY id DESC LIMIT {}",
            limit + 1
        ))?;
        let mut items = stmt.query_map(params![q.name, q.cursor], |r| json_row(r, "data"))?.collect::<rusqlite::Result<Vec<Event>>>()?;
        let cursor = if items.len() > limit {
            items.truncate(limit);
            items.last().map(|e| e.id.clone())
        } else {
            None
        };
        Ok(Page { items, cursor })
    }

    async fn wait_put(&self, w: &Wait) -> Result<()> {
        self.db().execute(
            "INSERT INTO waits (event, key, id, expires_at, data) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT DO UPDATE SET expires_at = excluded.expires_at, data = excluded.data",
            params![w.event, w.key.as_deref().unwrap_or(""), w.id, w.expires_at, serde_json::to_string(w)?],
        )?;
        Ok(())
    }

    async fn wait_delete(&self, w: &Wait) -> Result<()> {
        self.db().execute(
            "DELETE FROM waits WHERE event = ?1 AND key = ?2 AND id = ?3",
            params![w.event, w.key.as_deref().unwrap_or(""), w.id],
        )?;
        Ok(())
    }

    async fn waits(&self, event: &str, key: Option<&str>) -> Result<Vec<Wait>> {
        let db = self.db();
        let mut stmt = db.prepare_cached("SELECT data FROM waits WHERE event = ?1 AND key = ?2 AND expires_at > ?3 ORDER BY id")?;
        Ok(stmt
            .query_map(params![event, key.unwrap_or(""), now_ms()], |r| json_row(r, "data"))?
            .collect::<rusqlite::Result<_>>()?)
    }

    async fn usage_add(&self, day: i64, u: &Usage) -> Result<()> {
        let db = self.db();
        let tx = db.unchecked_transaction()?;
        for (field, n) in u.fields() {
            if n > 0 {
                tx.execute(
                    "INSERT INTO usage (day, field, n) VALUES (?1, ?2, ?3) ON CONFLICT DO UPDATE SET n = n + excluded.n",
                    params![day, field, n as i64],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    async fn usage(&self, from_day: i64) -> Result<Vec<(i64, Usage)>> {
        let db = self.db();
        let mut stmt = db.prepare_cached("SELECT day, field, n FROM usage WHERE day >= ?1 ORDER BY day")?;
        let mut out: Vec<(i64, Usage)> = Vec::new();
        for row in stmt.query_map([from_day], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))? {
            let (day, field, n) = row?;
            if out.last().is_none_or(|(d, _)| *d != day) {
                out.push((day, Usage::default()));
            }
            out.last_mut().unwrap().1.set(&field, n as u64);
        }
        Ok(out)
    }

    async fn sweep(&self, now: Ms) -> Result<u64> {
        let db = self.db();
        let tx = db.unchecked_transaction()?;
        let mut n = 0;
        n += tx.execute(
            "DELETE FROM bodies WHERE id IN (SELECT id FROM messages WHERE expires_at IS NOT NULL AND expires_at <= ?1)",
            [now],
        )?;
        tx.execute(
            "DELETE FROM attempts WHERE message_id IN (SELECT id FROM messages WHERE expires_at IS NOT NULL AND expires_at <= ?1)",
            [now],
        )?;
        n += tx.execute("DELETE FROM messages WHERE expires_at IS NOT NULL AND expires_at <= ?1", [now])?;
        tx.execute(
            "DELETE FROM steps WHERE run_id IN (SELECT id FROM runs WHERE expires_at IS NOT NULL AND expires_at <= ?1)",
            [now],
        )?;
        n += tx.execute("DELETE FROM runs WHERE expires_at IS NOT NULL AND expires_at <= ?1", [now])?;
        n += tx.execute("DELETE FROM events WHERE expires_at IS NOT NULL AND expires_at <= ?1", [now])?;
        n += tx.execute("DELETE FROM waits WHERE expires_at <= ?1", [now])?;
        n += tx.execute("DELETE FROM kv WHERE expires_at IS NOT NULL AND expires_at <= ?1", [now])?;
        tx.commit()?;
        Ok(n as u64)
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn conformance() {
        let store = std::sync::Arc::new(super::SqliteStore::memory(4).unwrap());
        super::super::conformance::run(store).await;
    }

    #[tokio::test]
    async fn conformance_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(super::SqliteStore::open(&dir.path().join("tasks.db"), 4).unwrap());
        super::super::conformance::run(store).await;
    }
}

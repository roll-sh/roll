//! control.db's automation tables. Every read-then-write is one `write` closure that only
//! touches the database (closures may re-run on Postgres); ids and tokens are made before it.

use super::registry::NewEvent;
use agent_proto::automation::{AutomationEventId, limits};
use code_sql::{Conn, OptionalExtension, params};

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS automations (
    id TEXT PRIMARY KEY,
    owner_id TEXT NOT NULL,
    -- AutomationSpec JSON.
    spec TEXT NOT NULL,
    enabled INTEGER NOT NULL,
    -- The trigger's source (manual, schedule, interval, webhook, code, agent, or an integration).
    source TEXT NOT NULL,
    revision INTEGER NOT NULL,
    -- The owner cursor of its last change.
    cursor INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    -- Deleted automations stay as tombstones (automation_removed on a reconnecting stream).
    deleted_at INTEGER,
    -- The inbound webhook's secret, sealed (ROLL_SECRET_KEY), and its last four characters.
    hook_secret TEXT,
    hook_hint TEXT,
    -- A schedule's or interval's next firing (unix ms).
    next_at INTEGER,
    -- RunBrief JSON of the newest run.
    last_run TEXT
);
CREATE INDEX IF NOT EXISTS automations_owner ON automations (owner_id, cursor);
CREATE INDEX IF NOT EXISTS automations_due ON automations (next_at);
CREATE TABLE IF NOT EXISTS automation_listens (
    -- What an enabled automation's trigger listens for, so a source finds its automations
    -- without reading every definition: (source, kind or '*', repo id or '*').
    automation_id TEXT NOT NULL,
    owner_id TEXT NOT NULL,
    source TEXT NOT NULL,
    kind TEXT NOT NULL,
    repo_id TEXT NOT NULL,
    PRIMARY KEY (automation_id, source, kind, repo_id)
);
CREATE INDEX IF NOT EXISTS automation_listens_source ON automation_listens (source, owner_id);
CREATE TABLE IF NOT EXISTS automation_secrets (
    automation_id TEXT NOT NULL,
    name TEXT NOT NULL,
    -- Sealed with ROLL_SECRET_KEY, bound to the automation and the name.
    sealed TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (automation_id, name)
);
CREATE TABLE IF NOT EXISTS automation_events (
    id TEXT PRIMARY KEY,
    owner_id TEXT NOT NULL,
    -- The idempotency key (<source>:<the event's own id>).
    key TEXT NOT NULL,
    source TEXT NOT NULL,
    kind TEXT NOT NULL,
    automation_id TEXT,
    data TEXT NOT NULL,
    summary TEXT NOT NULL,
    at INTEGER NOT NULL,
    -- new (not matched yet) | done
    state TEXT NOT NULL,
    UNIQUE (owner_id, key)
);
CREATE INDEX IF NOT EXISTS automation_events_new ON automation_events (state, owner_id, at);
CREATE TABLE IF NOT EXISTS automation_runs (
    id TEXT PRIMARY KEY,
    owner_id TEXT NOT NULL,
    automation_id TEXT NOT NULL,
    event_id TEXT NOT NULL,
    -- RunState.
    status TEXT NOT NULL,
    -- AutomationRun JSON (what the API and the stream show).
    json TEXT NOT NULL,
    -- The steps' private state between attempts, by step id (JSON).
    private TEXT NOT NULL DEFAULT '{}',
    cursor INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    -- When it's worked on next (unix ms); NULL once it's final.
    next_at INTEGER,
    -- Who is working on it (a random claim per attempt) and until when (unix ms).
    claim TEXT,
    claimed_until INTEGER,
    -- One run per automation and event.
    UNIQUE (automation_id, event_id)
);
CREATE INDEX IF NOT EXISTS automation_runs_owner ON automation_runs (owner_id, cursor);
CREATE INDEX IF NOT EXISTS automation_runs_due ON automation_runs (next_at);
CREATE INDEX IF NOT EXISTS automation_runs_of ON automation_runs (automation_id, created_at);
CREATE TABLE IF NOT EXISTS automation_threads (
    -- Threads runs started: their records wake the run, and the canvas tags them.
    machine_id TEXT NOT NULL,
    thread TEXT NOT NULL,
    owner_id TEXT NOT NULL,
    run_id TEXT NOT NULL,
    automation_id TEXT NOT NULL,
    PRIMARY KEY (machine_id, thread)
);
CREATE INDEX IF NOT EXISTS automation_threads_run ON automation_threads (run_id);
CREATE TABLE IF NOT EXISTS automation_tokens (
    -- sha256 of a token (rlr_…) made for one call a step makes to the API as its automation.
    hash TEXT PRIMARY KEY,
    owner_id TEXT NOT NULL,
    automation_id TEXT NOT NULL,
    -- The caller's token name: automation:<id>, or agent:<run_as>.
    name TEXT NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS automation_tokens_expiry ON automation_tokens (expires_at);
"#;

/// What a secret is sealed to: its automation and its name.
pub(crate) fn secret_scope(automation_id: &str, name: &str) -> String {
    format!("automation:{automation_id}:{name}")
}

pub(crate) fn secret(db: &code_sql::Db, automation_id: &str, name: &str) -> code_sql::Result<Option<String>> {
    db.query_row("SELECT sealed FROM automation_secrets WHERE automation_id = ? AND name = ?", [automation_id, name], |r| r.get(0)).optional()
}

/// Record an event once per key, in the caller's transaction.
pub(crate) fn insert_event(c: &Conn<'_>, owner_id: &str, e: &NewEvent) -> code_sql::Result<Option<AutomationEventId>> {
    let key = format!("{}:{}", e.source, e.id);
    let mut data = e.data.to_string();
    if data.len() > limits::EVENT {
        // Too big to keep whole: what matters is that it happened.
        data = serde_json::json!({ "truncated": true, "bytes": data.len() }).to_string();
    }
    let summary: String = e.summary.chars().take(200).collect();
    let id = AutomationEventId(crate::new_id(AutomationEventId::PREFIX.trim_end_matches('_')));
    let n = c.execute(
        "INSERT INTO automation_events (id, owner_id, key, source, kind, automation_id, data, summary, at, state) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'new') \
         ON CONFLICT (owner_id, key) DO NOTHING",
        params![id.as_str(), owner_id, key, e.source, e.kind, e.automation.as_ref().map(|a| a.as_str()), data, summary, code_sql::now_ms()],
    )?;
    Ok((n > 0).then_some(id))
}

pub(crate) fn insert_token(db: &code_sql::Db, hash: &str, owner_id: &str, automation_id: &str, name: &str, expires_at: i64) -> code_sql::Result<()> {
    db.write(|c| {
        c.execute("DELETE FROM automation_tokens WHERE expires_at < ?", [code_sql::now_ms()])?;
        c.execute(
            "INSERT INTO automation_tokens (hash, owner_id, automation_id, name, expires_at) VALUES (?, ?, ?, ?, ?)",
            params![hash, owner_id, automation_id, name, expires_at],
        )?;
        Ok(())
    })
}

pub(crate) fn delete_token(db: &code_sql::Db, hash: &str) -> code_sql::Result<()> {
    db.execute("DELETE FROM automation_tokens WHERE hash = ?", [hash]).map(|_| ())
}

/// A live call token's owner and caller name, if its automation still exists.
pub(crate) fn token(db: &code_sql::Db, hash: &str) -> code_sql::Result<Option<(String, String)>> {
    db.query_row(
        "SELECT t.owner_id, t.name FROM automation_tokens t JOIN automations a ON a.id = t.automation_id \
         WHERE t.hash = ? AND t.expires_at > ? AND a.deleted_at IS NULL",
        params![hash, code_sql::now_ms()],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
}

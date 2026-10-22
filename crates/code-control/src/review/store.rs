//! Reviews, threads, comments, review settings and requests, review apps and their deliveries
//! in control.db. Rows keep their insertion order through SQLite's rowid.

use super::{Comment, Review, Reviewer, ReviewerKind, Thread, Verdict};
use crate::db::{Db, now};
use anyhow::Result;
use rusqlite::{OptionalExtension, Row, params};

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS reviews (
    id TEXT PRIMARY KEY,
    change_id TEXT NOT NULL,
    repo_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    head TEXT NOT NULL,
    reviewer_kind TEXT NOT NULL,
    reviewer_name TEXT NOT NULL,
    reviewer_app TEXT,
    verdict TEXT NOT NULL,
    body TEXT NOT NULL,
    source TEXT NOT NULL,
    external_id TEXT,
    url TEXT,
    dismissed_at INTEGER,
    dismissed_by TEXT,
    dismiss_reason TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS reviews_change ON reviews (change_id);
CREATE UNIQUE INDEX IF NOT EXISTS reviews_external ON reviews (repo_id, source, external_id) WHERE external_id IS NOT NULL;
CREATE TABLE IF NOT EXISTS review_threads (
    id TEXT PRIMARY KEY,
    change_id TEXT NOT NULL,
    repo_id TEXT NOT NULL,
    review_id TEXT,
    path TEXT NOT NULL,
    side TEXT NOT NULL,
    original_line INTEGER NOT NULL,
    original_start_line INTEGER NOT NULL,
    original_revision INTEGER NOT NULL,
    line INTEGER NOT NULL,
    start_line INTEGER NOT NULL,
    revision INTEGER NOT NULL,
    -- The changeset the current lines are in (the change's head, or its base for `old`).
    at TEXT NOT NULL,
    outdated INTEGER NOT NULL DEFAULT 0,
    resolved_at INTEGER,
    resolved_by TEXT,
    source TEXT NOT NULL,
    external_id TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS review_threads_change ON review_threads (change_id);
CREATE UNIQUE INDEX IF NOT EXISTS review_threads_external ON review_threads (repo_id, source, external_id) WHERE external_id IS NOT NULL;
CREATE TABLE IF NOT EXISTS review_comments (
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL,
    repo_id TEXT NOT NULL,
    author_kind TEXT NOT NULL,
    author_name TEXT NOT NULL,
    author_app TEXT,
    body TEXT NOT NULL,
    source TEXT NOT NULL,
    external_id TEXT,
    deleted INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS review_comments_thread ON review_comments (thread_id);
CREATE UNIQUE INDEX IF NOT EXISTS review_comments_external ON review_comments (repo_id, source, external_id) WHERE external_id IS NOT NULL;
CREATE TABLE IF NOT EXISTS review_settings (
    repo_id TEXT PRIMARY KEY,
    rules TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
-- A review someone (roll's reviewer, a required review app) owes a change's head.
CREATE TABLE IF NOT EXISTS review_requests (
    change_id TEXT NOT NULL,
    reviewer_kind TEXT NOT NULL,
    reviewer_name TEXT NOT NULL,
    reviewer_app TEXT NOT NULL DEFAULT '',
    head TEXT NOT NULL,
    required INTEGER NOT NULL,
    requested_at INTEGER NOT NULL,
    done_at INTEGER,
    PRIMARY KEY (change_id, reviewer_kind, reviewer_name, reviewer_app, head)
);
CREATE TABLE IF NOT EXISTS review_apps (
    id TEXT PRIMARY KEY,
    owner_id TEXT NOT NULL,
    name TEXT NOT NULL,
    name_lower TEXT NOT NULL,
    webhook_url TEXT NOT NULL,
    events TEXT NOT NULL,
    repos TEXT NOT NULL,
    required INTEGER NOT NULL DEFAULT 0,
    active INTEGER NOT NULL DEFAULT 1,
    secret TEXT NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE (owner_id, name_lower)
);
CREATE TABLE IF NOT EXISTS review_deliveries (
    id TEXT PRIMARY KEY,
    app_id TEXT NOT NULL,
    event TEXT NOT NULL,
    payload TEXT NOT NULL,
    -- pending, delivered or failed (gave up)
    state TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    -- Unix milliseconds.
    next_at INTEGER NOT NULL,
    last_status INTEGER,
    last_error TEXT,
    created_at INTEGER NOT NULL,
    delivered_at INTEGER,
    -- The start of the last attempt's response body (LAST_RESPONSE_BYTES at most).
    last_response TEXT
);
CREATE INDEX IF NOT EXISTS review_deliveries_due ON review_deliveries (state, next_at);
CREATE INDEX IF NOT EXISTS review_deliveries_app ON review_deliveries (app_id);
"#;

fn reviewer(kind: String, name: String, app: Option<String>) -> Reviewer {
    Reviewer { kind: ReviewerKind::parse(&kind), name, app: app.filter(|a| !a.is_empty()) }
}

const REVIEW_COLS: &str = "id, change_id, revision, head, reviewer_kind, reviewer_name, reviewer_app, verdict, body, source, url, \
                           dismissed_at, dismissed_by, dismiss_reason, created_at, updated_at, external_id, rowid, ai";

fn review_row(r: &Row) -> rusqlite::Result<Review> {
    Ok(Review {
        id: r.get(0)?,
        change: r.get(1)?,
        revision: r.get::<_, i64>(2)? as u64,
        head: r.get(3)?,
        reviewer: reviewer(r.get(4)?, r.get(5)?, r.get(6)?),
        verdict: Verdict::parse(&r.get::<_, String>(7)?).unwrap_or(Verdict::Comment),
        body: r.get(8)?,
        source: r.get(9)?,
        url: r.get(10)?,
        dismissed: r.get::<_, Option<i64>>(11)?.is_some(),
        dismissed_by: r.get(12)?,
        dismiss_reason: r.get(13)?,
        created_at: r.get(14)?,
        updated_at: r.get(15)?,
        external_id: r.get(16)?,
        seq: r.get(17)?,
        ai: r.get::<_, Option<String>>(18)?.and_then(|a| serde_json::from_str(&a).ok()),
        stale: false,
        threads: None,
    })
}

const THREAD_COLS: &str = "id, change_id, review_id, path, side, original_line, original_start_line, original_revision, line, \
                           start_line, revision, at, outdated, resolved_at, resolved_by, source, created_at, updated_at, external_id";

fn thread_row(r: &Row) -> rusqlite::Result<Thread> {
    Ok(Thread {
        id: r.get(0)?,
        change: r.get(1)?,
        review: r.get(2)?,
        path: r.get(3)?,
        side: r.get(4)?,
        original_line: r.get::<_, i64>(5)? as u64,
        original_start_line: r.get::<_, i64>(6)? as u64,
        original_revision: r.get::<_, i64>(7)? as u64,
        line: r.get::<_, i64>(8)? as u64,
        start_line: r.get::<_, i64>(9)? as u64,
        revision: r.get::<_, i64>(10)? as u64,
        at: r.get(11)?,
        outdated: r.get::<_, i64>(12)? != 0,
        resolved: r.get::<_, Option<i64>>(13)?.is_some(),
        resolved_at: r.get(13)?,
        resolved_by: r.get(14)?,
        source: r.get(15)?,
        created_at: r.get(16)?,
        updated_at: r.get(17)?,
        external_id: r.get(18)?,
        comments: Vec::new(),
    })
}

const COMMENT_COLS: &str = "id, thread_id, author_kind, author_name, author_app, body, source, created_at, updated_at, external_id";

fn comment_row(r: &Row) -> rusqlite::Result<Comment> {
    let body: String = r.get(5)?;
    let author = reviewer(r.get(2)?, r.get(3)?, r.get(4)?);
    let finding = (author.kind == super::ReviewerKind::Roll)
        .then(|| code_review::engine::parse_finding(&body))
        .flatten()
        .map(|(severity, title, confidence)| serde_json::json!({ "severity": severity, "title": title, "confidence": confidence }));
    Ok(Comment {
        id: r.get(0)?,
        thread: r.get(1)?,
        author,
        finding,
        suggestion: super::lines::suggestion(&body),
        body,
        source: r.get(6)?,
        created_at: r.get(7)?,
        updated_at: r.get(8)?,
        external_id: r.get(9)?,
    })
}

/// A new review, as stored.
pub struct NewReview<'a> {
    pub id: &'a str,
    pub change_id: &'a str,
    pub repo_id: &'a str,
    pub revision: u64,
    pub head: &'a str,
    pub reviewer: &'a Reviewer,
    pub verdict: Verdict,
    pub body: &'a str,
    pub source: &'a str,
    pub external_id: Option<&'a str>,
    pub url: Option<&'a str>,
}

/// A new thread, as stored.
pub struct NewThread<'a> {
    pub id: &'a str,
    pub change_id: &'a str,
    pub repo_id: &'a str,
    pub review_id: Option<&'a str>,
    pub path: &'a str,
    pub side: &'a str,
    pub line: u64,
    pub start_line: u64,
    pub revision: u64,
    pub at: &'a str,
    pub source: &'a str,
    pub external_id: Option<&'a str>,
}

/// A review app, as stored.
#[derive(Clone, Debug)]
pub struct AppRow {
    pub id: String,
    pub owner_id: String,
    pub name: String,
    pub webhook_url: String,
    pub events: Vec<String>,
    /// Repo ids; empty: every repo of the owner.
    pub repos: Vec<String>,
    pub required: bool,
    pub active: bool,
    pub secret: String,
    pub created_at: i64,
    pub updated_at: i64,
}

const APP_COLS: &str = "id, owner_id, name, webhook_url, events, repos, required, active, secret, created_at, updated_at";

fn app_row(r: &Row) -> rusqlite::Result<AppRow> {
    Ok(AppRow {
        id: r.get(0)?,
        owner_id: r.get(1)?,
        name: r.get(2)?,
        webhook_url: r.get(3)?,
        events: serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or_default(),
        repos: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or_default(),
        required: r.get::<_, i64>(6)? != 0,
        active: r.get::<_, i64>(7)? != 0,
        secret: r.get(8)?,
        created_at: r.get(9)?,
        updated_at: r.get(10)?,
    })
}

#[derive(Clone, Debug)]
pub struct DeliveryRow {
    pub id: String,
    pub app_id: String,
    pub event: String,
    pub payload: String,
    pub state: String,
    pub attempts: i64,
    pub next_at: i64,
    pub last_status: Option<i64>,
    pub last_error: Option<String>,
    pub created_at: i64,
    pub delivered_at: Option<i64>,
    pub last_response: Option<String>,
}

const DELIVERY_COLS: &str = "id, app_id, event, payload, state, attempts, next_at, last_status, last_error, created_at, delivered_at, last_response";

fn delivery_row(r: &Row) -> rusqlite::Result<DeliveryRow> {
    Ok(DeliveryRow {
        id: r.get(0)?,
        app_id: r.get(1)?,
        event: r.get(2)?,
        payload: r.get(3)?,
        state: r.get(4)?,
        attempts: r.get(5)?,
        next_at: r.get(6)?,
        last_status: r.get(7)?,
        last_error: r.get(8)?,
        created_at: r.get(9)?,
        delivered_at: r.get(10)?,
        last_response: r.get(11)?,
    })
}

impl Db {
    // ── reviews ──

    pub fn insert_review(&self, n: &NewReview) -> Result<()> {
        let t = now();
        self.lock().execute(
            "INSERT INTO reviews (id, change_id, repo_id, revision, head, reviewer_kind, reviewer_name, reviewer_app, verdict, body, \
             source, external_id, url, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                n.id,
                n.change_id,
                n.repo_id,
                n.revision as i64,
                n.head,
                n.reviewer.kind.as_str(),
                n.reviewer.name,
                n.reviewer.app,
                n.verdict.as_str(),
                n.body,
                n.source,
                n.external_id,
                n.url,
                t,
                t
            ],
        )?;
        Ok(())
    }

    pub fn review(&self, id: &str) -> Result<Option<Review>> {
        Ok(self.lock().query_row(&format!("SELECT {REVIEW_COLS} FROM reviews WHERE id = ?"), [id], review_row).optional()?)
    }

    /// A review by its id at `source` (posted there by roll, or made there).
    pub fn review_by_external(&self, repo_id: &str, source: &str, external_id: &str) -> Result<Option<Review>> {
        Ok(self
            .lock()
            .query_row(
                &format!("SELECT {REVIEW_COLS} FROM reviews WHERE repo_id = ? AND (source = ? OR source = 'roll') AND external_id = ?"),
                params![repo_id, source, external_id],
                review_row,
            )
            .optional()?)
    }

    pub fn reviews(&self, change_id: &str) -> Result<Vec<Review>> {
        let db = self.lock();
        let mut st = db.prepare(&format!("SELECT {REVIEW_COLS} FROM reviews WHERE change_id = ? ORDER BY rowid"))?;
        Ok(st.query_map([change_id], review_row)?.collect::<Result<_, _>>()?)
    }

    pub fn update_review(&self, id: &str, verdict: Option<Verdict>, body: Option<&str>) -> Result<()> {
        let db = self.lock();
        if let Some(v) = verdict {
            db.execute("UPDATE reviews SET verdict = ?, updated_at = ? WHERE id = ?", params![v.as_str(), now(), id])?;
        }
        if let Some(b) = body {
            db.execute("UPDATE reviews SET body = ?, updated_at = ? WHERE id = ?", params![b, now(), id])?;
        }
        Ok(())
    }

    pub fn set_review_external(&self, id: &str, external_id: &str, url: Option<&str>) -> Result<()> {
        self.lock().execute(
            "UPDATE reviews SET external_id = ?, url = COALESCE(?, url), updated_at = ? WHERE id = ?",
            params![external_id, url, now(), id],
        )?;
        Ok(())
    }

    pub fn dismiss_review(&self, id: &str, by: &str, reason: &str) -> Result<bool> {
        let n = self.lock().execute(
            "UPDATE reviews SET dismissed_at = ?, dismissed_by = ?, dismiss_reason = ?, updated_at = ? WHERE id = ? AND dismissed_at IS NULL",
            params![now(), by, reason, now(), id],
        )?;
        Ok(n == 1)
    }

    // ── threads and comments ──

    pub fn insert_thread(&self, n: &NewThread) -> Result<()> {
        let t = now();
        self.lock().execute(
            "INSERT INTO review_threads (id, change_id, repo_id, review_id, path, side, original_line, original_start_line, \
             original_revision, line, start_line, revision, at, source, external_id, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                n.id,
                n.change_id,
                n.repo_id,
                n.review_id,
                n.path,
                n.side,
                n.line as i64,
                n.start_line as i64,
                n.revision as i64,
                n.line as i64,
                n.start_line as i64,
                n.revision as i64,
                n.at,
                n.source,
                n.external_id,
                t,
                t
            ],
        )?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn insert_comment(
        &self,
        id: &str,
        thread_id: &str,
        repo_id: &str,
        author: &Reviewer,
        body: &str,
        source: &str,
        external_id: Option<&str>,
    ) -> Result<()> {
        let t = now();
        let db = self.lock();
        db.execute(
            "INSERT INTO review_comments (id, thread_id, repo_id, author_kind, author_name, author_app, body, source, external_id, \
             created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![id, thread_id, repo_id, author.kind.as_str(), author.name, author.app, body, source, external_id, t, t],
        )?;
        db.execute("UPDATE review_threads SET updated_at = ? WHERE id = ?", params![t, thread_id])?;
        Ok(())
    }

    fn with_comments(&self, mut t: Thread) -> Result<Thread> {
        let db = self.lock();
        let mut st = db.prepare(&format!("SELECT {COMMENT_COLS} FROM review_comments WHERE thread_id = ? AND deleted = 0 ORDER BY rowid"))?;
        t.comments = st.query_map([&t.id], comment_row)?.collect::<Result<_, _>>()?;
        Ok(t)
    }

    pub fn thread(&self, id: &str) -> Result<Option<Thread>> {
        let t = self.lock().query_row(&format!("SELECT {THREAD_COLS} FROM review_threads WHERE id = ?"), [id], thread_row).optional()?;
        t.map(|t| self.with_comments(t)).transpose()
    }

    pub fn thread_by_external(&self, repo_id: &str, source: &str, external_id: &str) -> Result<Option<Thread>> {
        let t = self
            .lock()
            .query_row(
                &format!("SELECT {THREAD_COLS} FROM review_threads WHERE repo_id = ? AND (source = ? OR source = 'roll') AND external_id = ?"),
                params![repo_id, source, external_id],
                thread_row,
            )
            .optional()?;
        t.map(|t| self.with_comments(t)).transpose()
    }

    pub fn threads(&self, change_id: &str) -> Result<Vec<Thread>> {
        let rows: Vec<Thread> = {
            let db = self.lock();
            let mut st = db.prepare(&format!("SELECT {THREAD_COLS} FROM review_threads WHERE change_id = ? ORDER BY rowid"))?;
            st.query_map([change_id], thread_row)?.collect::<Result<_, _>>()?
        };
        rows.into_iter().map(|t| self.with_comments(t)).collect()
    }

    pub fn count_unresolved(&self, change_id: &str) -> Result<u64> {
        Ok(self.lock().query_row(
            "SELECT COUNT(*) FROM review_threads WHERE change_id = ? AND resolved_at IS NULL",
            [change_id],
            |r| r.get::<_, i64>(0),
        )? as u64)
    }

    pub fn set_thread_resolved(&self, id: &str, by: Option<&str>) -> Result<()> {
        let t = now();
        match by {
            Some(by) => self.lock().execute(
                "UPDATE review_threads SET resolved_at = COALESCE(resolved_at, ?), resolved_by = COALESCE(resolved_by, ?), updated_at = ? WHERE id = ?",
                params![t, by, t, id],
            )?,
            None => self.lock().execute(
                "UPDATE review_threads SET resolved_at = NULL, resolved_by = NULL, updated_at = ? WHERE id = ?",
                params![t, id],
            )?,
        };
        Ok(())
    }

    /// Move a thread to new lines of `revision` (or mark it outdated).
    pub fn move_thread(&self, id: &str, lines: Option<(u64, u64)>, revision: u64, at: &str) -> Result<()> {
        let t = now();
        match lines {
            Some((start, end)) => self.lock().execute(
                "UPDATE review_threads SET start_line = ?, line = ?, revision = ?, at = ?, updated_at = ? WHERE id = ?",
                params![start as i64, end as i64, revision as i64, at, t, id],
            )?,
            None => self.lock().execute("UPDATE review_threads SET outdated = 1, updated_at = ? WHERE id = ?", params![t, id])?,
        };
        Ok(())
    }

    pub fn comment_by_external(&self, repo_id: &str, source: &str, external_id: &str) -> Result<Option<Comment>> {
        Ok(self
            .lock()
            .query_row(
                &format!("SELECT {COMMENT_COLS} FROM review_comments WHERE repo_id = ? AND (source = ? OR source = 'roll') AND external_id = ?"),
                params![repo_id, source, external_id],
                comment_row,
            )
            .optional()?)
    }

    pub fn comment_thread(&self, comment_id: &str) -> Result<Option<String>> {
        Ok(self.lock().query_row("SELECT thread_id FROM review_comments WHERE id = ?", [comment_id], |r| r.get(0)).optional()?)
    }

    pub fn set_comment_external(&self, id: &str, external_id: &str) -> Result<()> {
        self.lock().execute("UPDATE review_comments SET external_id = ? WHERE id = ?", params![external_id, id])?;
        Ok(())
    }

    pub fn set_thread_external(&self, id: &str, external_id: &str) -> Result<()> {
        self.lock().execute("UPDATE review_threads SET external_id = ? WHERE id = ?", params![external_id, id])?;
        Ok(())
    }

    pub fn edit_comment(&self, id: &str, body: &str) -> Result<()> {
        self.lock().execute("UPDATE review_comments SET body = ?, updated_at = ? WHERE id = ?", params![body, now(), id])?;
        Ok(())
    }

    pub fn delete_comment(&self, id: &str) -> Result<()> {
        self.lock().execute("UPDATE review_comments SET deleted = 1, updated_at = ? WHERE id = ?", params![now(), id])?;
        Ok(())
    }

    // ── settings ──

    pub fn review_rules(&self, repo_id: &str) -> Result<Option<String>> {
        Ok(self.lock().query_row("SELECT rules FROM review_settings WHERE repo_id = ?", [repo_id], |r| r.get(0)).optional()?)
    }

    pub fn set_review_rules(&self, repo_id: &str, rules: &str) -> Result<()> {
        self.lock().execute(
            "INSERT INTO review_settings (repo_id, rules, updated_at) VALUES (?, ?, ?) \
             ON CONFLICT (repo_id) DO UPDATE SET rules = excluded.rules, updated_at = excluded.updated_at",
            params![repo_id, rules, now()],
        )?;
        Ok(())
    }

    // ── requests ──

    pub fn request_review(&self, change_id: &str, r: &Reviewer, head: &str, required: bool) -> Result<()> {
        self.lock().execute(
            "INSERT INTO review_requests (change_id, reviewer_kind, reviewer_name, reviewer_app, head, required, requested_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT DO UPDATE SET required = MAX(required, excluded.required), \
             done_at = NULL, requested_at = excluded.requested_at",
            params![change_id, r.kind.as_str(), r.name, r.app.clone().unwrap_or_default(), head, required, now()],
        )?;
        Ok(())
    }

    /// A reviewer gave (or no longer owes) a review of `head` (every head when `None`).
    pub fn finish_review_requests(&self, change_id: &str, r: &Reviewer, head: Option<&str>) -> Result<()> {
        let db = self.lock();
        let t = now();
        match head {
            Some(h) => db.execute(
                "UPDATE review_requests SET done_at = ? WHERE change_id = ? AND reviewer_kind = ? AND lower(reviewer_name) = lower(?) \
                 AND reviewer_app = ? AND head = ? AND done_at IS NULL",
                params![t, change_id, r.kind.as_str(), r.name, r.app.clone().unwrap_or_default(), h],
            )?,
            None => db.execute(
                "UPDATE review_requests SET done_at = ? WHERE change_id = ? AND reviewer_kind = ? AND lower(reviewer_name) = lower(?) \
                 AND reviewer_app = ? AND done_at IS NULL",
                params![t, change_id, r.kind.as_str(), r.name, r.app.clone().unwrap_or_default()],
            )?,
        };
        Ok(())
    }

    /// Required reviews still owed for `head`.
    pub fn pending_reviews(&self, change_id: &str, head: &str) -> Result<Vec<Reviewer>> {
        let db = self.lock();
        let mut st = db.prepare(
            "SELECT reviewer_kind, reviewer_name, reviewer_app FROM review_requests \
             WHERE change_id = ? AND head = ? AND required = 1 AND done_at IS NULL ORDER BY rowid",
        )?;
        let rows = st.query_map(params![change_id, head], |r| Ok(reviewer(r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // ── review apps ──

    pub fn insert_app(&self, a: &AppRow, token_hash: &str) -> Result<bool> {
        let n = self.lock().execute(
            "INSERT OR IGNORE INTO review_apps (id, owner_id, name, name_lower, webhook_url, events, repos, required, active, secret, \
             token_hash, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                a.id,
                a.owner_id,
                a.name,
                a.name.to_ascii_lowercase(),
                a.webhook_url,
                serde_json::to_string(&a.events)?,
                serde_json::to_string(&a.repos)?,
                a.required,
                a.active,
                a.secret,
                token_hash,
                a.created_at,
                a.updated_at
            ],
        )?;
        Ok(n == 1)
    }

    pub fn update_app(&self, a: &AppRow) -> Result<bool> {
        let n = self.lock().execute(
            "UPDATE OR IGNORE review_apps SET name = ?, name_lower = ?, webhook_url = ?, events = ?, repos = ?, required = ?, active = ?, \
             updated_at = ? WHERE id = ?",
            params![
                a.name,
                a.name.to_ascii_lowercase(),
                a.webhook_url,
                serde_json::to_string(&a.events)?,
                serde_json::to_string(&a.repos)?,
                a.required,
                a.active,
                now(),
                a.id
            ],
        )?;
        Ok(n == 1)
    }

    pub fn rotate_app(&self, id: &str, secret: &str, token_hash: &str) -> Result<()> {
        self.lock().execute(
            "UPDATE review_apps SET secret = ?, token_hash = ?, updated_at = ? WHERE id = ?",
            params![secret, token_hash, now(), id],
        )?;
        Ok(())
    }

    pub fn delete_app(&self, id: &str) -> Result<()> {
        let db = self.lock();
        db.execute("DELETE FROM review_apps WHERE id = ?", [id])?;
        db.execute("DELETE FROM review_deliveries WHERE app_id = ?", [id])?;
        Ok(())
    }

    pub fn app(&self, id: &str) -> Result<Option<AppRow>> {
        Ok(self.lock().query_row(&format!("SELECT {APP_COLS} FROM review_apps WHERE id = ?"), [id], app_row).optional()?)
    }

    pub fn app_by_token(&self, token_hash: &str) -> Result<Option<AppRow>> {
        Ok(self.lock().query_row(&format!("SELECT {APP_COLS} FROM review_apps WHERE token_hash = ?"), [token_hash], app_row).optional()?)
    }

    pub fn apps(&self, owner_id: Option<&str>) -> Result<Vec<AppRow>> {
        let db = self.lock();
        match owner_id {
            Some(o) => {
                let mut st = db.prepare(&format!("SELECT {APP_COLS} FROM review_apps WHERE owner_id = ? ORDER BY name_lower"))?;
                Ok(st.query_map([o], app_row)?.collect::<Result<_, _>>()?)
            }
            None => {
                let mut st = db.prepare(&format!("SELECT {APP_COLS} FROM review_apps ORDER BY name_lower"))?;
                Ok(st.query_map([], app_row)?.collect::<Result<_, _>>()?)
            }
        }
    }

    // ── deliveries ──

    pub fn insert_delivery(&self, id: &str, app_id: &str, event: &str, payload: &str) -> Result<()> {
        self.lock().execute(
            "INSERT INTO review_deliveries (id, app_id, event, payload, state, next_at, created_at) VALUES (?, ?, ?, ?, 'pending', ?, ?)",
            params![id, app_id, event, payload, code_store::now_ms(), now()],
        )?;
        Ok(())
    }

    pub fn delivery(&self, id: &str) -> Result<Option<DeliveryRow>> {
        Ok(self.lock().query_row(&format!("SELECT {DELIVERY_COLS} FROM review_deliveries WHERE id = ?"), [id], delivery_row).optional()?)
    }

    pub fn deliveries(&self, app_id: &str, limit: usize) -> Result<Vec<DeliveryRow>> {
        let db = self.lock();
        let mut st = db.prepare(&format!("SELECT {DELIVERY_COLS} FROM review_deliveries WHERE app_id = ? ORDER BY rowid DESC LIMIT ?"))?;
        Ok(st.query_map(params![app_id, limit as i64], delivery_row)?.collect::<Result<_, _>>()?)
    }

    /// Pending deliveries due by `now_ms`, oldest first.
    pub fn due_deliveries(&self, now_ms: i64, limit: usize) -> Result<Vec<DeliveryRow>> {
        let db = self.lock();
        let mut st = db.prepare(&format!(
            "SELECT {DELIVERY_COLS} FROM review_deliveries WHERE state = 'pending' AND next_at <= ? ORDER BY next_at, rowid LIMIT ?"
        ))?;
        Ok(st.query_map(params![now_ms, limit as i64], delivery_row)?.collect::<Result<_, _>>()?)
    }

    /// When the next pending delivery is due.
    pub fn next_delivery_at(&self) -> Result<Option<i64>> {
        Ok(self.lock().query_row("SELECT MIN(next_at) FROM review_deliveries WHERE state = 'pending'", [], |r| r.get(0))?)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn finish_delivery(
        &self,
        id: &str,
        state: &str,
        attempts: i64,
        next_at: i64,
        status: Option<i64>,
        error: Option<&str>,
        response: Option<&str>,
    ) -> Result<()> {
        let delivered = (state == "delivered").then(now);
        self.lock().execute(
            "UPDATE review_deliveries SET state = ?, attempts = ?, next_at = ?, last_status = ?, last_error = ?, \
             delivered_at = COALESCE(?, delivered_at), last_response = ? WHERE id = ?",
            params![state, attempts, next_at, status, error, delivered, response, id],
        )?;
        Ok(())
    }

    pub fn redeliver(&self, id: &str) -> Result<()> {
        self.lock().execute(
            "UPDATE review_deliveries SET state = 'pending', next_at = ? WHERE id = ?",
            params![code_store::now_ms(), id],
        )?;
        Ok(())
    }
}

//! roll's reviewer in control.db: per-repo reviewer settings, the job queue, metered usage,
//! learnings, owners' own provider keys (sealed) and spending caps, paused changes.

use crate::db::{Db, now};
use anyhow::Result;
use code_sql::{OptionalExtension, Row, params};
use serde::Serialize;
use serde_json::Value;

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS reviewer_settings (
    repo_id TEXT PRIMARY KEY,
    settings TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS ai_jobs (
    id TEXT PRIMARY KEY,
    repo_id TEXT NOT NULL,
    change_id TEXT NOT NULL,
    -- review or command
    kind TEXT NOT NULL,
    mode TEXT,
    -- Quoted: FULL is a reserved word in Postgres.
    "full" INTEGER NOT NULL DEFAULT 0,
    command TEXT,
    -- queued, running, done, failed or skipped
    state TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    -- Unix milliseconds: not before.
    run_at INTEGER NOT NULL DEFAULT 0,
    error TEXT,
    review_id TEXT,
    requested_by TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    -- What a running job is doing (indexing, chunking, reviewing, verifying, posting).
    stage TEXT,
    -- {"groups_done", "groups_total"} while reviewing.
    progress TEXT
);
CREATE INDEX IF NOT EXISTS ai_jobs_state ON ai_jobs (state, run_at);
CREATE INDEX IF NOT EXISTS ai_jobs_change ON ai_jobs (change_id);
CREATE TABLE IF NOT EXISTS ai_usage (
    id TEXT PRIMARY KEY,
    owner_id TEXT NOT NULL,
    repo_id TEXT NOT NULL,
    change_id TEXT,
    review_id TEXT,
    kind TEXT NOT NULL,
    mode TEXT NOT NULL,
    chosen_by TEXT NOT NULL,
    reasons TEXT NOT NULL,
    speed TEXT NOT NULL,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    key_source TEXT NOT NULL,
    files_reviewed INTEGER NOT NULL,
    input_tokens INTEGER NOT NULL,
    output_tokens INTEGER NOT NULL,
    cache_write_tokens INTEGER NOT NULL,
    cache_read_tokens INTEGER NOT NULL,
    provider_cost_usd REAL,
    list_usd REAL NOT NULL,
    price_usd REAL NOT NULL,
    free INTEGER NOT NULL,
    capped INTEGER NOT NULL,
    human_author INTEGER NOT NULL,
    author TEXT NOT NULL,
    public INTEGER NOT NULL,
    month TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS ai_usage_owner_month ON ai_usage (owner_id, month);
CREATE TABLE IF NOT EXISTS ai_learnings (
    id TEXT PRIMARY KEY,
    repo_id TEXT NOT NULL,
    path TEXT,
    text TEXT NOT NULL,
    source TEXT NOT NULL,
    created_by TEXT NOT NULL,
    thread_id TEXT,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS ai_learnings_repo ON ai_learnings (repo_id);
CREATE TABLE IF NOT EXISTS owner_review_providers (
    owner_id TEXT PRIMARY KEY,
    provider TEXT NOT NULL,
    base_url TEXT,
    model TEXT,
    secondary_model TEXT,
    key_sealed TEXT NOT NULL,
    key_last4 TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS owner_billing (
    owner_id TEXT PRIMARY KEY,
    monthly_cap_usd REAL,
    on_cap TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS ai_paused (
    change_id TEXT PRIMARY KEY,
    paused_by TEXT NOT NULL,
    paused_at INTEGER NOT NULL
);
"#;

#[derive(Clone, Debug, Serialize)]
pub struct Job {
    pub id: String,
    pub repo_id: String,
    pub change_id: String,
    pub kind: String,
    pub mode: Option<String>,
    pub full: bool,
    #[serde(skip)]
    pub command: Option<Value>,
    pub state: String,
    pub attempts: u32,
    pub error: Option<String>,
    pub review_id: Option<String>,
    pub requested_by: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    /// `queued | indexing | chunking | reviewing | verifying | posting | done | failed`.
    pub stage: String,
    /// `{groups_done, groups_total}` once the diff is split into groups.
    pub progress: Option<Value>,
}

const JOB_COLS: &str = "id, repo_id, change_id, kind, mode, \"full\", command, state, attempts, error, review_id, requested_by, created_at, updated_at, stage, progress";

/// The stage a job shows: a running job's own, otherwise what its state says.
fn stage_of(state: &str, stored: Option<String>) -> String {
    match state {
        "running" => stored.unwrap_or_else(|| "chunking".into()),
        "queued" => "queued".into(),
        "failed" => "failed".into(),
        _ => "done".into(),
    }
}

fn job_row(r: &Row<'_>) -> code_sql::Result<Job> {
    Ok(Job {
        id: r.get(0)?,
        repo_id: r.get(1)?,
        change_id: r.get(2)?,
        kind: r.get(3)?,
        mode: r.get(4)?,
        full: r.get::<_, i64>(5)? != 0,
        command: r.get::<_, Option<String>>(6)?.and_then(|s| serde_json::from_str(&s).ok()),
        state: r.get(7)?,
        attempts: r.get::<_, i64>(8)? as u32,
        error: r.get(9)?,
        review_id: r.get(10)?,
        requested_by: r.get(11)?,
        created_at: r.get(12)?,
        updated_at: r.get(13)?,
        stage: stage_of(&r.get::<_, String>(7)?, r.get(14)?),
        progress: r.get::<_, Option<String>>(15)?.and_then(|s| serde_json::from_str(&s).ok()),
    })
}

#[derive(Clone, Debug, Serialize)]
pub struct Learning {
    pub id: String,
    pub repo_id: String,
    pub path: Option<String>,
    pub text: String,
    /// `command` (`@roll don't flag …`), `resolved` (a roll thread resolved with a reason),
    /// `dismissed` (a roll review dismissed with a reason) or `api`.
    pub source: String,
    pub created_by: String,
    pub thread: Option<String>,
    pub created_at: i64,
}

fn learning_row(r: &Row<'_>) -> code_sql::Result<Learning> {
    Ok(Learning {
        id: r.get(0)?,
        repo_id: r.get(1)?,
        path: r.get(2)?,
        text: r.get(3)?,
        source: r.get(4)?,
        created_by: r.get(5)?,
        thread: r.get(6)?,
        created_at: r.get(7)?,
    })
}

/// An owner's own provider (the key stays sealed).
#[derive(Clone, Debug)]
pub struct OwnerProvider {
    pub provider: String,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub secondary_model: Option<String>,
    pub key_sealed: String,
    pub key_last4: String,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Billing {
    pub monthly_cap_usd: Option<f64>,
    /// `pause`, `byok_fallback` or `standard_only`.
    pub on_cap: String,
    pub updated_at: Option<i64>,
}

/// One metered review or reply.
#[derive(Clone, Debug, Serialize)]
pub struct UsageRow {
    pub id: String,
    pub owner_id: String,
    pub repo_id: String,
    pub change_id: Option<String>,
    pub review_id: Option<String>,
    pub kind: String,
    pub mode: String,
    pub chosen_by: String,
    pub reasons: Vec<String>,
    pub speed: String,
    pub provider: String,
    pub model: String,
    pub key_source: String,
    pub files_reviewed: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_read_tokens: u64,
    pub provider_cost_usd: Option<f64>,
    pub list_usd: f64,
    pub price_usd: f64,
    pub free: bool,
    pub capped: bool,
    pub human_author: bool,
    pub author: String,
    pub public: bool,
    pub month: String,
    pub created_at: i64,
}

const USAGE_COLS: &str = "id, owner_id, repo_id, change_id, review_id, kind, mode, chosen_by, reasons, speed, provider, model, key_source, files_reviewed, \
    input_tokens, output_tokens, cache_write_tokens, cache_read_tokens, provider_cost_usd, list_usd, price_usd, free, capped, human_author, author, public, month, created_at";

fn usage_row(r: &Row<'_>) -> code_sql::Result<UsageRow> {
    Ok(UsageRow {
        id: r.get(0)?,
        owner_id: r.get(1)?,
        repo_id: r.get(2)?,
        change_id: r.get(3)?,
        review_id: r.get(4)?,
        kind: r.get(5)?,
        mode: r.get(6)?,
        chosen_by: r.get(7)?,
        reasons: serde_json::from_str(&r.get::<_, String>(8)?).unwrap_or_default(),
        speed: r.get(9)?,
        provider: r.get(10)?,
        model: r.get(11)?,
        key_source: r.get(12)?,
        files_reviewed: r.get::<_, i64>(13)? as u32,
        input_tokens: r.get::<_, i64>(14)? as u64,
        output_tokens: r.get::<_, i64>(15)? as u64,
        cache_write_tokens: r.get::<_, i64>(16)? as u64,
        cache_read_tokens: r.get::<_, i64>(17)? as u64,
        provider_cost_usd: r.get(18)?,
        list_usd: r.get(19)?,
        price_usd: r.get(20)?,
        free: r.get::<_, i64>(21)? != 0,
        capped: r.get::<_, i64>(22)? != 0,
        human_author: r.get::<_, i64>(23)? != 0,
        author: r.get(24)?,
        public: r.get::<_, i64>(25)? != 0,
        month: r.get(26)?,
        created_at: r.get(27)?,
    })
}

impl Db {
    // ── settings ──

    pub fn reviewer_settings(&self, repo_id: &str) -> Result<Option<Value>> {
        let s: Option<String> = self.sql().query_row("SELECT settings FROM reviewer_settings WHERE repo_id = ?", [repo_id], |r| r.get(0)).optional()?;
        Ok(s.and_then(|s| serde_json::from_str(&s).ok()))
    }

    pub fn set_reviewer_settings(&self, repo_id: &str, v: &Value) -> Result<()> {
        self.sql().execute(
            "INSERT INTO reviewer_settings (repo_id, settings, updated_at) VALUES (?, ?, ?) \
             ON CONFLICT (repo_id) DO UPDATE SET settings = excluded.settings, updated_at = excluded.updated_at",
            params![repo_id, v.to_string(), now()],
        )?;
        Ok(())
    }

    // ── jobs ──

    #[allow(clippy::too_many_arguments)]
    pub fn insert_ai_job(&self, id: &str, repo_id: &str, change_id: &str, kind: &str, mode: Option<&str>, full: bool, command: Option<&Value>, by: Option<&str>) -> Result<()> {
        let t = now();
        self.sql().execute(
            "INSERT INTO ai_jobs (id, repo_id, change_id, kind, mode, \"full\", command, state, requested_by, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, 'queued', ?, ?, ?)",
            params![id, repo_id, change_id, kind, mode, full as i64, command.map(|c| c.to_string()), by, t, t],
        )?;
        Ok(())
    }

    /// A queued review of the change, if one is waiting (a new revision folds into it).
    pub fn queued_review_job(&self, change_id: &str) -> Result<Option<Job>> {
        Ok(self
            .sql()
            .query_row(&format!("SELECT {JOB_COLS} FROM ai_jobs WHERE change_id = ? AND kind = 'review' AND state = 'queued' ORDER BY rowid LIMIT 1"), [change_id], job_row)
            .optional()?)
    }

    pub fn ai_job(&self, id: &str) -> Result<Option<Job>> {
        Ok(self.sql().query_row(&format!("SELECT {JOB_COLS} FROM ai_jobs WHERE id = ?"), [id], job_row).optional()?)
    }

    /// Up to `n` jobs that may run now (none of their change's running), oldest first.
    pub fn due_ai_jobs(&self, n: usize) -> Result<Vec<Job>> {
        let ms = crate::db::now_ms();
        Ok(self.sql().query(
            &format!(
                "SELECT {JOB_COLS} FROM ai_jobs WHERE state = 'queued' AND run_at <= ? \
                 AND NOT EXISTS (SELECT 1 FROM ai_jobs o WHERE o.change_id = ai_jobs.change_id AND o.state = 'running') ORDER BY created_at, rowid LIMIT ?"
            ),
            params![ms, crate::db::limit(n)],
            job_row,
        )?)
    }

    /// Take a queued job; `false` if someone else did, or another job of its change is running
    /// (one at a time per change, so a re-review sees the review before it).
    pub fn claim_ai_job(&self, id: &str) -> Result<bool> {
        Ok(self.sql().execute(
            "UPDATE ai_jobs SET state = 'running', stage = NULL, progress = NULL, attempts = attempts + 1, updated_at = ? WHERE id = ? AND state = 'queued' \
             AND NOT EXISTS (SELECT 1 FROM ai_jobs o WHERE o.change_id = ai_jobs.change_id AND o.state = 'running')",
            params![now(), id],
        )? == 1)
    }

    /// What a running job is doing now.
    pub fn set_ai_stage(&self, id: &str, stage: &str, progress: Option<&Value>) -> Result<()> {
        self.sql().execute(
            "UPDATE ai_jobs SET stage = ?, progress = COALESCE(?, progress), updated_at = ? WHERE id = ? AND state = 'running'",
            params![stage, progress.map(|p| p.to_string()), now(), id],
        )?;
        Ok(())
    }

    pub fn finish_ai_job(&self, id: &str, state: &str, error: Option<&str>, review_id: Option<&str>) -> Result<()> {
        self.sql().execute(
            "UPDATE ai_jobs SET state = ?, error = ?, review_id = COALESCE(?, review_id), updated_at = ? WHERE id = ?",
            params![state, error, review_id, now(), id],
        )?;
        Ok(())
    }

    /// Put a failed attempt back in line, not before `run_at_ms`.
    pub fn retry_ai_job(&self, id: &str, error: &str, run_at_ms: i64) -> Result<()> {
        self.sql().execute("UPDATE ai_jobs SET state = 'queued', error = ?, run_at = ?, updated_at = ? WHERE id = ?", params![error, run_at_ms, now(), id])?;
        Ok(())
    }

    /// After a restart: jobs that were running start over.
    pub fn requeue_running_ai_jobs(&self) -> Result<usize> {
        Ok(self.sql().execute("UPDATE ai_jobs SET state = 'queued', updated_at = ? WHERE state = 'running'", [now()])?)
    }

    /// `(queued, running)` for a repo, and its latest finished job.
    pub fn ai_job_summary(&self, repo_id: &str) -> Result<(u64, u64, Option<Job>)> {
        let c = self.sql();
        let n = |state: &str| c.query_row("SELECT COUNT(*) FROM ai_jobs WHERE repo_id = ? AND state = ?", params![repo_id, state], |r| r.get::<_, i64>(0)).map(|x| x as u64);
        let (q, r) = (n("queued")?, n("running")?);
        let last = c
            .query_row(
                &format!("SELECT {JOB_COLS} FROM ai_jobs WHERE repo_id = ? AND state IN ('done', 'failed', 'skipped') ORDER BY updated_at DESC, rowid DESC LIMIT 1"),
                [repo_id],
                job_row,
            )
            .optional()?;
        Ok((q, r, last))
    }

    pub fn change_ai_jobs(&self, change_id: &str) -> Result<Vec<Job>> {
        Ok(self.sql().query(&format!("SELECT {JOB_COLS} FROM ai_jobs WHERE change_id = ? ORDER BY rowid"), [change_id], job_row)?)
    }

    // ── paused ──

    pub fn set_ai_paused(&self, change_id: &str, by: Option<&str>) -> Result<()> {
        match by {
            Some(b) => self.sql().execute(
                "INSERT INTO ai_paused (change_id, paused_by, paused_at) VALUES (?, ?, ?) \
                 ON CONFLICT (change_id) DO UPDATE SET paused_by = excluded.paused_by, paused_at = excluded.paused_at",
                params![change_id, b, now()],
            )?,
            None => self.sql().execute("DELETE FROM ai_paused WHERE change_id = ?", [change_id])?,
        };
        Ok(())
    }

    pub fn ai_paused(&self, change_id: &str) -> Result<bool> {
        Ok(self.sql().query_row("SELECT COUNT(*) FROM ai_paused WHERE change_id = ?", [change_id], |r| r.get::<_, i64>(0))? > 0)
    }

    // ── usage ──

    pub fn insert_usage(&self, u: &UsageRow) -> Result<()> {
        self.sql().execute(
            &format!("INSERT INTO ai_usage ({USAGE_COLS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"),
            params![
                u.id,
                u.owner_id,
                u.repo_id,
                u.change_id,
                u.review_id,
                u.kind,
                u.mode,
                u.chosen_by,
                serde_json::to_string(&u.reasons).unwrap_or_else(|_| "[]".into()),
                u.speed,
                u.provider,
                u.model,
                u.key_source,
                u.files_reviewed as i64,
                u.input_tokens as i64,
                u.output_tokens as i64,
                u.cache_write_tokens as i64,
                u.cache_read_tokens as i64,
                u.provider_cost_usd,
                u.list_usd,
                u.price_usd,
                u.free as i64,
                u.capped as i64,
                u.human_author as i64,
                u.author,
                u.public as i64,
                u.month,
                u.created_at
            ],
        )?;
        Ok(())
    }

    pub fn usage_rows(&self, owner_id: &str, month: &str) -> Result<Vec<UsageRow>> {
        Ok(self.sql().query(
            &format!("SELECT {USAGE_COLS} FROM ai_usage WHERE owner_id = ? AND month = ? ORDER BY created_at DESC, rowid DESC"),
            params![owner_id, month],
            usage_row,
        )?)
    }

    /// The month so far: charged total, free public reviews used, and this author's hosted
    /// standard charges.
    pub fn usage_month(&self, owner_id: &str, month: &str, author: &str) -> Result<(f64, u32, f64)> {
        let c = self.sql();
        let total: f64 = c.query_row("SELECT COALESCE(SUM(price_usd), 0) FROM ai_usage WHERE owner_id = ? AND month = ?", params![owner_id, month], |r| r.get(0))?;
        let free: i64 = c.query_row("SELECT COUNT(*) FROM ai_usage WHERE owner_id = ? AND month = ? AND free = 1", params![owner_id, month], |r| r.get(0))?;
        let author_std: f64 = c.query_row(
            "SELECT COALESCE(SUM(price_usd), 0) FROM ai_usage WHERE owner_id = ? AND month = ? AND author = ? AND human_author = 1 \
             AND kind = 'review' AND mode = 'standard' AND key_source = 'hosted'",
            params![owner_id, month, author],
            |r| r.get(0),
        )?;
        Ok((total, free as u32, author_std))
    }

    // ── learnings ──

    #[allow(clippy::too_many_arguments)]
    pub fn insert_learning(&self, id: &str, repo_id: &str, path: Option<&str>, text: &str, source: &str, by: &str, thread: Option<&str>) -> Result<()> {
        self.sql().execute(
            "INSERT INTO ai_learnings (id, repo_id, path, text, source, created_by, thread_id, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            params![id, repo_id, path, text, source, by, thread, now()],
        )?;
        Ok(())
    }

    pub fn learnings(&self, repo_id: &str) -> Result<Vec<Learning>> {
        Ok(self.sql().query(
            "SELECT id, repo_id, path, text, source, created_by, thread_id, created_at FROM ai_learnings WHERE repo_id = ? ORDER BY id",
            [repo_id],
            learning_row,
        )?)
    }

    pub fn delete_learning(&self, repo_id: &str, id: &str) -> Result<bool> {
        Ok(self.sql().execute("DELETE FROM ai_learnings WHERE repo_id = ? AND id = ?", params![repo_id, id])? == 1)
    }

    pub fn learning_for_thread(&self, thread_id: &str) -> Result<bool> {
        Ok(self.sql().query_row("SELECT COUNT(*) FROM ai_learnings WHERE thread_id = ?", [thread_id], |r| r.get::<_, i64>(0))? > 0)
    }

    // ── owners' providers and billing ──

    pub fn owner_provider(&self, owner_id: &str) -> Result<Option<OwnerProvider>> {
        Ok(self
            .sql()
            .query_row(
                "SELECT provider, base_url, model, secondary_model, key_sealed, key_last4, updated_at FROM owner_review_providers WHERE owner_id = ?",
                [owner_id],
                |r| {
                    Ok(OwnerProvider {
                        provider: r.get(0)?,
                        base_url: r.get(1)?,
                        model: r.get(2)?,
                        secondary_model: r.get(3)?,
                        key_sealed: r.get(4)?,
                        key_last4: r.get(5)?,
                        updated_at: r.get(6)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn set_owner_provider(&self, owner_id: &str, p: Option<&OwnerProvider>) -> Result<()> {
        match p {
            Some(p) => self.sql().execute(
                "INSERT INTO owner_review_providers (owner_id, provider, base_url, model, secondary_model, key_sealed, key_last4, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT (owner_id) DO UPDATE SET provider = excluded.provider, base_url = excluded.base_url, \
                 model = excluded.model, secondary_model = excluded.secondary_model, key_sealed = excluded.key_sealed, \
                 key_last4 = excluded.key_last4, updated_at = excluded.updated_at",
                params![owner_id, p.provider, p.base_url, p.model, p.secondary_model, p.key_sealed, p.key_last4, now()],
            )?,
            None => self.sql().execute("DELETE FROM owner_review_providers WHERE owner_id = ?", [owner_id])?,
        };
        Ok(())
    }

    pub fn billing(&self, owner_id: &str) -> Result<Billing> {
        Ok(self
            .sql()
            .query_row("SELECT monthly_cap_usd, on_cap, updated_at FROM owner_billing WHERE owner_id = ?", [owner_id], |r| {
                Ok(Billing { monthly_cap_usd: r.get(0)?, on_cap: r.get(1)?, updated_at: Some(r.get(2)?) })
            })
            .optional()?
            .unwrap_or(Billing { monthly_cap_usd: None, on_cap: "pause".into(), updated_at: None }))
    }

    pub fn set_billing(&self, owner_id: &str, cap: Option<f64>, on_cap: &str) -> Result<()> {
        self.sql().execute(
            "INSERT INTO owner_billing (owner_id, monthly_cap_usd, on_cap, updated_at) VALUES (?, ?, ?, ?) \
             ON CONFLICT (owner_id) DO UPDATE SET monthly_cap_usd = excluded.monthly_cap_usd, on_cap = excluded.on_cap, updated_at = excluded.updated_at",
            params![owner_id, cap, on_cap, now()],
        )?;
        Ok(())
    }

    // ── what the author did lately (for `auto`) ──

    /// The author's changes reverted, and ejected from the queue, since `since`.
    pub fn author_history(&self, repo_id: &str, author: &str, since: i64) -> Result<(u32, u32)> {
        let c = self.sql();
        let reverts: i64 = c.query_row(
            "SELECT COUNT(*) FROM changes r WHERE r.repo_id = ?1 AND r.created_at >= ?3 AND lower(r.title) LIKE 'revert: %' \
             AND EXISTS (SELECT 1 FROM changes o WHERE o.repo_id = ?1 AND o.author = ?2 AND r.title = 'Revert: ' || o.title)",
            params![repo_id, author, since],
            |r| r.get(0),
        )?;
        let ejected: i64 = c.query_row(
            "SELECT COUNT(*) FROM queue_entries q JOIN changes c ON c.id = q.change_id WHERE c.repo_id = ? AND c.author = ? AND q.state = 'ejected' AND q.enqueued_at >= ?",
            params![repo_id, author, since],
            |r| r.get(0),
        )?;
        Ok((reverts as u32, ejected as u32))
    }

    // ── reviews' `ai` ──

    pub fn set_review_ai(&self, review_id: &str, ai: &Value) -> Result<()> {
        self.sql().execute("UPDATE reviews SET ai = ? WHERE id = ?", params![ai.to_string(), review_id])?;
        Ok(())
    }
}

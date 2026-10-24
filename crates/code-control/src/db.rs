//! control.db: owners, tokens, repos, changes, work items, checks. Written once in SQLite's
//! dialect through code-sql, so it lives in SQLite (one node) or Postgres (many nodes, all
//! writing at once). Code itself lives in code-store's code.db.
//!
//! Every access is a [`code_sql::Db::read`] (autocommit reads) or a [`code_sql::Db::write`] (one
//! serializable transaction, re-run on Postgres after a conflict). Anything that reads and then
//! writes on what it read (numbering, claims, queue positions) does both inside one `write`, so
//! it stays atomic with other nodes writing. Write closures only touch the database: callers act
//! (emit, cache, notify) after `write` returns.

use crate::model::*;
use anyhow::Result;
use code_sql::{Backend, Conn, OptionalExtension, Row, params};
use serde_json::{Value, json};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS owners (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    kind TEXT NOT NULL,
    display_name TEXT NOT NULL,
    bio TEXT,
    url TEXT,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS owner_tokens (
    hash TEXT PRIMARY KEY,
    owner_id TEXT NOT NULL,
    name TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    id TEXT,
    prefix TEXT,
    last_used_at INTEGER
);
CREATE TABLE IF NOT EXISTS repos (
    id TEXT PRIMARY KEY,
    owner_id TEXT NOT NULL,
    name TEXT NOT NULL,
    name_lower TEXT NOT NULL,
    description TEXT,
    visibility TEXT NOT NULL,
    source TEXT NOT NULL,
    default_branch TEXT NOT NULL,
    land_policy TEXT NOT NULL,
    required_checks TEXT NOT NULL DEFAULT '[]',
    github TEXT,
    import TEXT,
    next_number INTEGER NOT NULL DEFAULT 1,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE (owner_id, name_lower)
);
CREATE TABLE IF NOT EXISTS repo_redirects (
    owner_id TEXT NOT NULL,
    name_lower TEXT NOT NULL,
    repo_id TEXT NOT NULL,
    PRIMARY KEY (owner_id, name_lower)
);
CREATE TABLE IF NOT EXISTS changes (
    id TEXT PRIMARY KEY,
    repo_id TEXT NOT NULL,
    number INTEGER NOT NULL,
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    author TEXT NOT NULL,
    state TEXT NOT NULL,
    base TEXT NOT NULL,
    head TEXT NOT NULL,
    revision INTEGER NOT NULL,
    work_item TEXT,
    branch TEXT,
    github_pr INTEGER,
    github_url TEXT,
    trusted INTEGER NOT NULL DEFAULT 1,
    preview TEXT,
    landed_as TEXT,
    conflict TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE (repo_id, number)
);
CREATE INDEX IF NOT EXISTS changes_repo_state ON changes (repo_id, state, number);
CREATE INDEX IF NOT EXISTS changes_branch ON changes (repo_id, branch);
CREATE INDEX IF NOT EXISTS changes_work ON changes (work_item);
CREATE TABLE IF NOT EXISTS change_revisions (
    change_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    base TEXT NOT NULL,
    head TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (change_id, revision)
);
CREATE TABLE IF NOT EXISTS checks (
    change_id TEXT NOT NULL,
    name TEXT NOT NULL,
    status TEXT NOT NULL,
    source TEXT NOT NULL,
    url TEXT,
    summary TEXT,
    started_at INTEGER,
    finished_at INTEGER,
    PRIMARY KEY (change_id, name)
);
CREATE TABLE IF NOT EXISTS work_items (
    id TEXT PRIMARY KEY,
    repo_id TEXT NOT NULL,
    number INTEGER NOT NULL,
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    labels TEXT NOT NULL DEFAULT '[]',
    priority INTEGER NOT NULL,
    state TEXT NOT NULL,
    source TEXT NOT NULL,
    github_url TEXT,
    claimed_by TEXT,
    lease_until INTEGER,
    -- Length of the current lease, so heartbeats renew it by the same amount.
    lease_s INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE (repo_id, number)
);
CREATE INDEX IF NOT EXISTS work_repo_state ON work_items (repo_id, state, priority, created_at);
CREATE UNIQUE INDEX IF NOT EXISTS work_source ON work_items (repo_id, source) WHERE source <> 'native';
-- The merge queue. A change has at most one active entry; finished ones stay as its history.
CREATE TABLE IF NOT EXISTS queue_entries (
    id TEXT PRIMARY KEY,
    change_id TEXT NOT NULL,
    repo_id TEXT NOT NULL,
    -- FIFO order within a lane (re-queueing goes to the back).
    seq INTEGER NOT NULL,
    lane TEXT NOT NULL,
    state TEXT NOT NULL,
    error TEXT,
    batch_id TEXT,
    -- While landing: the tested changeset this change lands as (crash recovery).
    landed_as TEXT,
    enqueued_by TEXT,
    enqueued_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    finished_at INTEGER
);
CREATE UNIQUE INDEX IF NOT EXISTS queue_active ON queue_entries (change_id) WHERE state NOT IN ('ejected', 'landed');
CREATE INDEX IF NOT EXISTS queue_repo ON queue_entries (repo_id, state, seq);
CREATE INDEX IF NOT EXISTS queue_change ON queue_entries (change_id, enqueued_at);
-- Per-repo queue and CI state.
CREATE TABLE IF NOT EXISTS repo_queue (
    repo_id TEXT PRIMARY KEY,
    paused INTEGER NOT NULL DEFAULT 0,
    -- Adaptive batch size (NULL: the configured maximum).
    batch_size INTEGER,
    -- The newest default-branch head whose checks all passed.
    last_green TEXT,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS audit (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id TEXT NOT NULL,
    at INTEGER NOT NULL,
    actor TEXT NOT NULL,
    action TEXT NOT NULL,
    detail TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS audit_repo ON audit (repo_id, id);
"#;

pub struct Db {
    sql: code_sql::Db,
}

pub fn now() -> i64 {
    code_store::now_ms() / 1000
}

pub fn now_ms() -> i64 {
    code_store::now_ms()
}

const REPO_COLS: &str = "r.id, r.owner_id, o.name, r.name, r.description, r.visibility, r.source, r.default_branch, \
                         r.land_policy, r.required_checks, r.github, r.import, r.created_at, r.updated_at";

fn repo_row(r: &Row<'_>) -> code_sql::Result<RepoRow> {
    Ok(RepoRow {
        id: r.get(0)?,
        owner_id: r.get(1)?,
        owner: r.get(2)?,
        name: r.get(3)?,
        description: r.get(4)?,
        visibility: Visibility::parse(&r.get::<_, String>(5)?),
        source: r.get(6)?,
        default_branch: r.get(7)?,
        land_policy: LandPolicy::parse(&r.get::<_, String>(8)?).unwrap_or(LandPolicy::PostMerge),
        required_checks: serde_json::from_str(&r.get::<_, String>(9)?).unwrap_or_default(),
        github: r.get::<_, Option<String>>(10)?.and_then(|s| serde_json::from_str(&s).ok()),
        import: r.get::<_, Option<String>>(11)?.and_then(|s| serde_json::from_str(&s).ok()),
        created_at: r.get(12)?,
        updated_at: r.get(13)?,
    })
}

const OWNER_COLS: &str =
    "o.id, o.name, o.kind, o.display_name, o.bio, o.url, o.created_at, (SELECT COUNT(*) FROM repos r WHERE r.owner_id = o.id)";

fn owner_row(r: &Row<'_>) -> code_sql::Result<Owner> {
    Ok(Owner {
        id: r.get(0)?,
        name: r.get(1)?,
        kind: OwnerKind::parse(&r.get::<_, String>(2)?),
        display_name: r.get(3)?,
        bio: r.get(4)?,
        url: r.get(5)?,
        created_at: r.get(6)?,
        repos_count: r.get::<_, i64>(7)? as u64,
    })
}

/// A change row without its checks (and repo full name), filled in by the caller.
const CHANGE_COLS: &str = "id, repo_id, number, title, body, author, state, base, head, revision, work_item, branch, github_pr, \
                           github_url, preview, landed_as, conflict, created_at, updated_at, trusted, draft, \
                           (run_approved_head IS NOT NULL AND run_approved_head = head)";

fn change_row(r: &Row<'_>) -> code_sql::Result<Change> {
    Ok(Change {
        id: r.get(0)?,
        repo_id: r.get(1)?,
        repo: String::new(),
        number: r.get::<_, i64>(2)? as u64,
        title: r.get(3)?,
        body: r.get(4)?,
        author: r.get(5)?,
        state: ChangeState::parse(&r.get::<_, String>(6)?).unwrap_or(ChangeState::Open),
        base: r.get(7)?,
        head: r.get(8)?,
        revision: r.get::<_, i64>(9)? as u64,
        work_item: r.get(10)?,
        branch: r.get(11)?,
        github_pr: r.get::<_, Option<i64>>(12)?.map(|n| n as u64),
        github_url: r.get(13)?,
        checks: Vec::new(),
        preview: r.get::<_, Option<String>>(14)?.and_then(|s| serde_json::from_str(&s).ok()),
        landed_as: r.get(15)?,
        conflict: r.get::<_, Option<String>>(16)?.and_then(|s| serde_json::from_str(&s).ok()),
        created_at: r.get(17)?,
        updated_at: r.get(18)?,
        trusted: r.get::<_, i64>(19)? != 0,
        draft: r.get::<_, i64>(20)? != 0,
        run_approved: r.get::<_, i64>(21)? != 0,
        queue: None,
        blockers: Vec::new(),
        review: None,
    })
}

/// A queue entry as stored.
#[derive(Clone, Debug)]
pub struct EntryRow {
    pub id: String,
    pub change_id: String,
    pub repo_id: String,
    pub seq: i64,
    pub lane: Lane,
    pub state: EntryState,
    pub error: Option<String>,
    pub batch_id: Option<String>,
    pub landed_as: Option<String>,
    pub enqueued_by: Option<String>,
    pub enqueued_at: i64,
    pub finished_at: Option<i64>,
    /// The log tail behind `error` (failing checks' output), when there is one.
    pub output: Option<String>,
}

const ENTRY_COLS: &str =
    "id, change_id, repo_id, seq, lane, state, error, batch_id, landed_as, enqueued_by, enqueued_at, finished_at, output";

fn entry_row(r: &Row<'_>) -> code_sql::Result<EntryRow> {
    Ok(EntryRow {
        id: r.get(0)?,
        change_id: r.get(1)?,
        repo_id: r.get(2)?,
        seq: r.get(3)?,
        lane: Lane::parse(&r.get::<_, String>(4)?).unwrap_or(Lane::Normal),
        state: EntryState::parse(&r.get::<_, String>(5)?).unwrap_or(EntryState::Waiting),
        error: r.get(6)?,
        batch_id: r.get(7)?,
        landed_as: r.get(8)?,
        enqueued_by: r.get(9)?,
        enqueued_at: r.get(10)?,
        finished_at: r.get(11)?,
        output: r.get(12)?,
    })
}

/// Active entries in queue order: the batch being tested, then the waiting ones; urgent before
/// normal within each (the order a batch is built in), FIFO within a lane.
const QUEUE_RANK: &str = "(CASE WHEN state = 'waiting' THEN 2 ELSE 0 END + CASE lane WHEN 'urgent' THEN 0 ELSE 1 END)";
const QUEUE_ORDER: &str = "(CASE WHEN state = 'waiting' THEN 2 ELSE 0 END + CASE lane WHEN 'urgent' THEN 0 ELSE 1 END), seq";

#[derive(Clone, Debug, Default)]
pub struct RepoQueue {
    pub paused: bool,
    pub batch_size: Option<usize>,
    pub last_green: Option<String>,
}

const WORK_COLS: &str =
    "id, repo_id, number, title, body, labels, priority, state, source, github_url, claimed_by, lease_until, created_at, updated_at";

fn work_row(r: &Row<'_>) -> code_sql::Result<WorkItem> {
    Ok(WorkItem {
        id: r.get(0)?,
        repo_id: r.get(1)?,
        repo: String::new(),
        number: r.get::<_, i64>(2)? as u64,
        title: r.get(3)?,
        body: r.get(4)?,
        labels: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or_default(),
        priority: r.get(6)?,
        state: WorkState::parse(&r.get::<_, String>(7)?).unwrap_or(WorkState::Open),
        source: r.get(8)?,
        github_url: r.get(9)?,
        claimed_by: r.get(10)?,
        lease_until: r.get(11)?,
        changes: Vec::new(),
        created_at: r.get(12)?,
        updated_at: r.get(13)?,
    })
}

fn json(v: &impl serde::Serialize) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "null".into())
}

/// A row limit as a parameter. `usize::MAX` (no limit) must stay positive: SQLite reads a
/// negative LIMIT as none, Postgres refuses it.
pub(crate) fn limit(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// Fields of a new work item.
pub struct NewWork<'a> {
    pub title: &'a str,
    pub body: &'a str,
    pub labels: &'a [String],
    pub priority: i64,
    pub source: &'a str,
    pub github_url: Option<&'a str>,
    pub state: WorkState,
}

/// Fields of a new change.
pub struct NewChange<'a> {
    pub title: &'a str,
    pub body: &'a str,
    pub author: &'a str,
    pub base: &'a str,
    pub head: &'a str,
    pub work_item: Option<&'a str>,
    pub branch: Option<&'a str>,
    pub trusted: bool,
    pub draft: bool,
}

/// Columns added after their table first shipped: `(table, column, declaration)`. Added on open
/// when missing; idempotent, so several nodes may start at once.
const COLUMNS: &[(&str, &str, &str)] = &[
    // Databases from before roll's reviewer recorded how a review was made.
    ("reviews", "ai", "TEXT"),
    // Databases from before `changes.trusted`.
    ("changes", "trusted", "INTEGER NOT NULL DEFAULT 1"),
    // Databases from before drafts and approved runs.
    ("changes", "draft", "INTEGER NOT NULL DEFAULT 0"),
    ("changes", "run_approved_head", "TEXT"),
    // Databases from before queue entries kept a failure's output apart from its reason.
    ("queue_entries", "output", "TEXT"),
    // Databases from before AI jobs reported their stage.
    ("ai_jobs", "stage", "TEXT"),
    ("ai_jobs", "progress", "TEXT"),
    // Databases from before webhooks remembered where in the land log they started.
    ("repo_webhooks", "since_seq", "INTEGER NOT NULL DEFAULT 0"),
    // Databases from before deliveries kept the start of the response.
    ("review_deliveries", "last_response", "TEXT"),
    // Databases from before tokens could be listed and revoked.
    ("owner_tokens", "id", "TEXT"),
    ("owner_tokens", "prefix", "TEXT"),
    ("owner_tokens", "last_used_at", "INTEGER"),
];

impl Db {
    /// Open control.db in `sql`: create what's missing and bring old databases up to date.
    /// Every node of a cluster may do this at once.
    pub fn open(sql: code_sql::Db) -> Result<Db> {
        for schema in [SCHEMA, crate::review::store::SCHEMA, crate::ai::store::SCHEMA, crate::webhooks::SCHEMA, crate::broken::SCHEMA, crate::mcp::SCHEMA] {
            sql.ensure_schema(schema)?;
        }
        for (table, column, decl) in COLUMNS {
            sql.add_column(table, column, decl)?;
        }
        // Old tokens get an id (idempotent: only rows without one), then ids are unique.
        sql.execute("UPDATE owner_tokens SET id = 'tok_' || substr(hash, 1, 14) WHERE id IS NULL", ())?;
        sql.ensure_schema("CREATE UNIQUE INDEX IF NOT EXISTS owner_tokens_id ON owner_tokens (id)")?;
        // `required_checks` was the queue's old name.
        sql.execute("UPDATE repos SET land_policy = 'queue' WHERE land_policy = 'required_checks'", ())?;
        Ok(Db { sql })
    }

    /// The database itself (tests, and statements of their own).
    pub fn sql(&self) -> &code_sql::Db {
        &self.sql
    }

    pub fn backend(&self) -> Backend {
        self.sql.backend()
    }

    /// One serializable transaction, durable when this returns. `f` may run more than once
    /// (Postgres conflicts), so it must only touch the database. (Reads go through
    /// [`code_sql::Db::query`] and friends: autocommit, each statement seeing the latest commit.)
    pub(crate) fn write<T>(&self, f: impl FnMut(&Conn<'_>) -> code_sql::Result<T>) -> Result<T> {
        Ok(self.sql.write(f)?)
    }

    // ── owners ──

    pub fn insert_owner(&self, o: &Owner) -> Result<bool> {
        let n = self.sql.execute(
            "INSERT OR IGNORE INTO owners (id, name, kind, display_name, bio, url, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
            params![o.id, o.name, o.kind.as_str(), o.display_name, o.bio, o.url, o.created_at],
        )?;
        Ok(n == 1)
    }

    pub fn owner(&self, name: &str) -> Result<Option<Owner>> {
        Ok(self.sql.query_row(&format!("SELECT {OWNER_COLS} FROM owners o WHERE o.name = ?"), [name.to_ascii_lowercase()], owner_row).optional()?)
    }

    pub fn owner_by_id(&self, id: &str) -> Result<Option<Owner>> {
        Ok(self.sql.query_row(&format!("SELECT {OWNER_COLS} FROM owners o WHERE o.id = ?"), [id], owner_row).optional()?)
    }

    pub fn owners(&self) -> Result<Vec<Owner>> {
        Ok(self.sql.query(&format!("SELECT {OWNER_COLS} FROM owners o ORDER BY o.name"), (), owner_row)?)
    }

    pub fn update_owner(&self, id: &str, display_name: Option<&str>, bio: Option<&str>, url: Option<&str>) -> Result<()> {
        self.write(|c| {
            if let Some(v) = display_name {
                c.execute("UPDATE owners SET display_name = ? WHERE id = ?", params![v, id])?;
            }
            if let Some(v) = bio {
                c.execute("UPDATE owners SET bio = NULLIF(?, '') WHERE id = ?", params![v, id])?;
            }
            if let Some(v) = url {
                c.execute("UPDATE owners SET url = NULLIF(?, '') WHERE id = ?", params![v, id])?;
            }
            Ok(())
        })
    }

    pub fn insert_token(&self, hash: &str, id: &str, prefix: &str, owner_id: &str, name: &str) -> Result<()> {
        self.sql.execute(
            "INSERT INTO owner_tokens (hash, owner_id, name, created_at, id, prefix) VALUES (?, ?, ?, ?, ?, ?)",
            params![hash, owner_id, name, now(), id, prefix],
        )?;
        Ok(())
    }

    /// The owner and name of a token. Notes its use (at most once a minute per token), so a
    /// lookup only writes once a minute.
    pub fn token_owner(&self, hash: &str) -> Result<Option<(String, String)>> {
        let found: Option<(String, String, Option<i64>)> = self
            .sql
            .query_row("SELECT owner_id, name, last_used_at FROM owner_tokens WHERE hash = ?", [hash], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?;
        let Some((owner_id, name, last_used)) = found else { return Ok(None) };
        let t = now();
        if last_used.is_none_or(|at| at < t - 60) {
            self.sql.execute(
                "UPDATE owner_tokens SET last_used_at = ? WHERE hash = ? AND (last_used_at IS NULL OR last_used_at < ?)",
                params![t, hash, t - 60],
            )?;
        }
        Ok(Some((owner_id, name)))
    }

    /// An owner's tokens (never the tokens themselves), newest first.
    pub fn tokens(&self, owner_id: &str) -> Result<Vec<Value>> {
        Ok(self.sql.query(
            "SELECT id, name, created_at, last_used_at, prefix FROM owner_tokens WHERE owner_id = ? ORDER BY created_at DESC, id",
            [owner_id],
            |r| {
                Ok(json!({
                    "id": r.get::<_, String>(0)?,
                    "name": r.get::<_, String>(1)?,
                    "created_at": r.get::<_, i64>(2)?,
                    "last_used_at": r.get::<_, Option<i64>>(3)?,
                    "prefix": r.get::<_, Option<String>>(4)?,
                }))
            },
        )?)
    }

    /// Revoke an owner's token by id; whether there was one.
    pub fn delete_token(&self, owner_id: &str, id: &str) -> Result<bool> {
        Ok(self.sql.execute("DELETE FROM owner_tokens WHERE owner_id = ? AND id = ?", params![owner_id, id])? > 0)
    }

    // ── repos ──

    /// Insert a repo; `false` when the owner already has one by that name.
    pub fn insert_repo(&self, r: &RepoRow) -> Result<bool> {
        let (name_lower, required, github, import) =
            (r.name.to_ascii_lowercase(), json(&r.required_checks), r.github.as_ref().map(json), r.import.as_ref().map(json));
        self.write(|c| {
            let n = c.execute(
                "INSERT OR IGNORE INTO repos (id, owner_id, name, name_lower, description, visibility, source, default_branch, \
                 land_policy, required_checks, github, import, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    r.id,
                    r.owner_id,
                    r.name,
                    name_lower,
                    r.description,
                    r.visibility.as_str(),
                    r.source,
                    r.default_branch,
                    r.land_policy.as_str(),
                    required,
                    github,
                    import,
                    r.created_at,
                    r.updated_at
                ],
            )?;
            if n == 1 {
                // The name now belongs to this repo, not to whatever was renamed away from it.
                c.execute("DELETE FROM repo_redirects WHERE owner_id = ? AND name_lower = ?", params![r.owner_id, name_lower])?;
            }
            Ok(n == 1)
        })
    }

    pub fn repo(&self, owner_id: &str, name: &str) -> Result<Option<RepoRow>> {
        Ok(self
            .sql
            .query_row(
                &format!("SELECT {REPO_COLS} FROM repos r JOIN owners o ON o.id = r.owner_id WHERE r.owner_id = ? AND r.name_lower = ?"),
                params![owner_id, name.to_ascii_lowercase()],
                repo_row,
            )
            .optional()?)
    }

    pub fn repo_by_id(&self, id: &str) -> Result<Option<RepoRow>> {
        Ok(self
            .sql
            .query_row(&format!("SELECT {REPO_COLS} FROM repos r JOIN owners o ON o.id = r.owner_id WHERE r.id = ?"), [id], repo_row)
            .optional()?)
    }

    pub fn redirect(&self, owner_id: &str, name: &str) -> Result<Option<String>> {
        Ok(self
            .sql
            .query_row(
                "SELECT repo_id FROM repo_redirects WHERE owner_id = ? AND name_lower = ?",
                params![owner_id, name.to_ascii_lowercase()],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn repos(&self, owner_id: Option<&str>) -> Result<Vec<RepoRow>> {
        let sql = format!(
            "SELECT {REPO_COLS} FROM repos r JOIN owners o ON o.id = r.owner_id WHERE (CAST(?1 AS TEXT) IS NULL OR r.owner_id = ?1) \
             ORDER BY r.updated_at DESC, o.name, r.name_lower"
        );
        Ok(self.sql.query(&sql, [owner_id], repo_row)?)
    }

    pub fn count_repos(&self) -> Result<u64> {
        Ok(self.sql.query_row("SELECT COUNT(*) FROM repos", (), |r| r.get::<_, i64>(0))? as u64)
    }

    /// Rename, keeping a redirect from the old name. `false` when the new name is taken.
    pub fn rename_repo(&self, r: &RepoRow, new_name: &str) -> Result<bool> {
        let (new_lower, old_lower) = (new_name.to_ascii_lowercase(), r.name.to_ascii_lowercase());
        self.write(|c| {
            let taken = c
                .query_row("SELECT 1 FROM repos WHERE owner_id = ? AND name_lower = ? AND id <> ?", params![r.owner_id, new_lower, r.id], |_| Ok(()))
                .optional()?
                .is_some();
            if taken {
                return Ok(false);
            }
            c.execute("UPDATE repos SET name = ?, name_lower = ?, updated_at = ? WHERE id = ?", params![new_name, new_lower, now(), r.id])?;
            c.execute("DELETE FROM repo_redirects WHERE owner_id = ? AND name_lower = ?", params![r.owner_id, new_lower])?;
            if !r.name.eq_ignore_ascii_case(new_name) {
                c.execute(
                    "INSERT INTO repo_redirects (owner_id, name_lower, repo_id) VALUES (?, ?, ?) \
                     ON CONFLICT (owner_id, name_lower) DO UPDATE SET repo_id = excluded.repo_id",
                    params![r.owner_id, old_lower, r.id],
                )?;
            }
            Ok(true)
        })
    }

    pub fn update_repo_settings(&self, r: &RepoRow) -> Result<()> {
        self.sql.execute(
            "UPDATE repos SET description = ?, visibility = ?, default_branch = ?, land_policy = ?, required_checks = ?, \
             github = ?, import = ?, updated_at = ? WHERE id = ?",
            params![
                r.description,
                r.visibility.as_str(),
                r.default_branch,
                r.land_policy.as_str(),
                json(&r.required_checks),
                r.github.as_ref().map(json),
                r.import.as_ref().map(json),
                now(),
                r.id
            ],
        )?;
        Ok(())
    }

    pub fn touch_repo(&self, id: &str) -> Result<()> {
        self.sql.execute("UPDATE repos SET updated_at = ? WHERE id = ?", params![now(), id])?;
        Ok(())
    }

    pub fn delete_repo(&self, id: &str) -> Result<()> {
        self.write(|c| {
            c.execute("DELETE FROM checks WHERE change_id IN (SELECT id FROM changes WHERE repo_id = ?)", [id])?;
            c.execute("DELETE FROM change_revisions WHERE change_id IN (SELECT id FROM changes WHERE repo_id = ?)", [id])?;
            c.execute("DELETE FROM review_requests WHERE change_id IN (SELECT id FROM changes WHERE repo_id = ?)", [id])?;
            c.execute("DELETE FROM changes WHERE repo_id = ?", [id])?;
            c.execute("DELETE FROM reviews WHERE repo_id = ?", [id])?;
            c.execute("DELETE FROM review_threads WHERE repo_id = ?", [id])?;
            c.execute("DELETE FROM review_comments WHERE repo_id = ?", [id])?;
            c.execute("DELETE FROM review_settings WHERE repo_id = ?", [id])?;
            c.execute("DELETE FROM work_items WHERE repo_id = ?", [id])?;
            c.execute("DELETE FROM repo_redirects WHERE repo_id = ?", [id])?;
            c.execute("DELETE FROM queue_entries WHERE repo_id = ?", [id])?;
            c.execute("DELETE FROM repo_queue WHERE repo_id = ?", [id])?;
            c.execute("DELETE FROM audit WHERE repo_id = ?", [id])?;
            c.execute("DELETE FROM repos WHERE id = ?", [id])?;
            Ok(())
        })
    }

    // ── changes ──

    /// The repo's next number (changes and work items share one sequence). Inside a write: the
    /// row lock (Postgres) or the single writer (SQLite) keeps numbers unique.
    fn next_number(c: &Conn<'_>, repo_id: &str) -> code_sql::Result<i64> {
        c.query_row("UPDATE repos SET next_number = next_number + 1 WHERE id = ? RETURNING next_number - 1", [repo_id], |r| r.get(0))
    }

    pub fn insert_change(&self, id: &str, repo_id: &str, n: &NewChange) -> Result<()> {
        self.write(|c| {
            let number = Self::next_number(c, repo_id)?;
            let t = now();
            c.execute(
                "INSERT INTO changes (id, repo_id, number, title, body, author, state, base, head, revision, work_item, branch, \
                 trusted, draft, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, 'open', ?, ?, 1, ?, ?, ?, ?, ?, ?)",
                params![id, repo_id, number, n.title, n.body, n.author, n.base, n.head, n.work_item, n.branch, n.trusted, n.draft, t, t],
            )?;
            c.execute("INSERT INTO change_revisions (change_id, revision, base, head, created_at) VALUES (?, 1, ?, ?, ?)", params![id, n.base, n.head, t])?;
            Ok(())
        })
    }

    pub fn change(&self, id: &str) -> Result<Option<Change>> {
        let row = self.sql.query_row(&format!("SELECT {CHANGE_COLS} FROM changes WHERE id = ?"), [id], change_row).optional()?;
        row.map(|c| self.with_checks(c)).transpose()
    }

    pub fn change_by_number(&self, repo_id: &str, number: u64) -> Result<Option<Change>> {
        let row = self
            .sql
            .query_row(&format!("SELECT {CHANGE_COLS} FROM changes WHERE repo_id = ? AND number = ?"), params![repo_id, number as i64], change_row)
            .optional()?;
        row.map(|c| self.with_checks(c)).transpose()
    }

    /// The open (not landed or closed) change pushed as `branch`.
    pub fn change_by_branch(&self, repo_id: &str, branch: &str) -> Result<Option<Change>> {
        let row = self
            .sql
            .query_row(
                &format!(
                    "SELECT {CHANGE_COLS} FROM changes WHERE repo_id = ? AND branch = ? AND state NOT IN ('landed', 'closed') \
                     ORDER BY number DESC LIMIT 1"
                ),
                params![repo_id, branch],
                change_row,
            )
            .optional()?;
        row.map(|c| self.with_checks(c)).transpose()
    }

    pub fn changes(&self, repo_id: &str, state: Option<ChangeState>, limit: usize) -> Result<Vec<Change>> {
        let rows = self.sql.query(
            &format!("SELECT {CHANGE_COLS} FROM changes WHERE repo_id = ?1 AND (CAST(?2 AS TEXT) IS NULL OR state = ?2) ORDER BY number DESC LIMIT ?3"),
            params![repo_id, state.map(|s| s.as_str()), crate::db::limit(limit)],
            change_row,
        )?;
        rows.into_iter().map(|c| self.with_checks(c)).collect()
    }

    /// Open changes in every repo, oldest first: what a restarted daemon resumes checks for.
    pub fn open_changes(&self, limit: usize) -> Result<Vec<Change>> {
        let rows = self.sql.query(
            &format!("SELECT {CHANGE_COLS} FROM changes WHERE state = 'open' ORDER BY created_at LIMIT ?1"),
            [crate::db::limit(limit)],
            change_row,
        )?;
        rows.into_iter().map(|c| self.with_checks(c)).collect()
    }

    /// Changes that have a preview, in every repo.
    pub fn changes_with_preview(&self) -> Result<Vec<Change>> {
        let rows = self.sql.query(&format!("SELECT {CHANGE_COLS} FROM changes WHERE preview IS NOT NULL ORDER BY created_at"), (), change_row)?;
        rows.into_iter().map(|c| self.with_checks(c)).collect()
    }

    pub fn count_changes(&self, repo_id: &str, states: &[&str]) -> Result<u64> {
        let list = states.iter().map(|s| format!("'{s}'")).collect::<Vec<_>>().join(",");
        let n: i64 = self.sql.query_row(&format!("SELECT COUNT(*) FROM changes WHERE repo_id = ? AND state IN ({list})"), [repo_id], |r| r.get(0))?;
        Ok(n as u64)
    }

    fn with_checks(&self, mut c: Change) -> Result<Change> {
        c.checks = self.checks(&c.id)?;
        Ok(c)
    }

    pub fn changes_for_work(&self, work_id: &str) -> Result<Vec<String>> {
        Ok(self.sql.query("SELECT id FROM changes WHERE work_item = ? ORDER BY number", [work_id], |r| r.get(0))?)
    }

    pub fn revise_change(&self, id: &str, base: &str, head: &str) -> Result<()> {
        self.write(|c| {
            let t = now();
            let rev: i64 = c.query_row(
                "UPDATE changes SET base = ?, head = ?, revision = revision + 1, state = 'open', conflict = NULL, updated_at = ? \
                 WHERE id = ? RETURNING revision",
                params![base, head, t, id],
                |r| r.get(0),
            )?;
            c.execute("INSERT INTO change_revisions (change_id, revision, base, head, created_at) VALUES (?, ?, ?, ?, ?)", params![id, rev, base, head, t])?;
            // Checks belong to a revision: a new one starts without results.
            c.execute("DELETE FROM checks WHERE change_id = ?", [id])?;
            Ok(())
        })
    }

    pub fn set_change_state(&self, id: &str, state: ChangeState, landed_as: Option<&str>, conflict: Option<&ConflictInfo>) -> Result<()> {
        self.sql.execute(
            "UPDATE changes SET state = ?, landed_as = COALESCE(?, landed_as), conflict = ?, updated_at = ? WHERE id = ?",
            params![state.as_str(), landed_as, conflict.map(json), now(), id],
        )?;
        Ok(())
    }

    pub fn set_change_preview(&self, id: &str, preview: Option<&Preview>) -> Result<()> {
        self.sql.execute("UPDATE changes SET preview = ?, updated_at = ? WHERE id = ?", params![preview.map(json), now(), id])?;
        Ok(())
    }

    // ── checks ──

    pub fn checks(&self, change_id: &str) -> Result<Vec<Check>> {
        Ok(self.sql.query(
            "SELECT name, status, source, url, summary, started_at, finished_at FROM checks WHERE change_id = ? ORDER BY name",
            [change_id],
            |r| {
                Ok(Check {
                    name: r.get(0)?,
                    status: CheckStatus::parse(&r.get::<_, String>(1)?).unwrap_or(CheckStatus::Queued),
                    source: r.get(2)?,
                    url: r.get(3)?,
                    summary: r.get(4)?,
                    started_at: r.get(5)?,
                    finished_at: r.get(6)?,
                })
            },
        )?)
    }

    /// Record a check. With `head`, only while the change is still at that head (`false`
    /// otherwise): a result for an older revision must not count for the new one.
    pub fn upsert_check(&self, change_id: &str, head: Option<&str>, ch: &Check) -> Result<bool> {
        self.write(|c| {
            if let Some(head) = head {
                let current: Option<String> = c.query_row("SELECT head FROM changes WHERE id = ?", [change_id], |r| r.get(0)).optional()?;
                if current.as_deref() != Some(head) {
                    return Ok(false);
                }
            }
            c.execute(
                "INSERT INTO checks (change_id, name, status, source, url, summary, started_at, finished_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT (change_id, name) DO UPDATE SET status = excluded.status, source = excluded.source, \
                 url = COALESCE(excluded.url, checks.url), summary = COALESCE(excluded.summary, checks.summary), \
                 started_at = COALESCE(checks.started_at, excluded.started_at), finished_at = excluded.finished_at",
                params![change_id, ch.name, ch.status.as_str(), ch.source, ch.url, ch.summary, ch.started_at, ch.finished_at],
            )?;
            Ok(true)
        })
    }

    // ── work items ──

    /// Claimed items whose lease ran out go back to `open` (inside a write).
    fn expire_in(c: &Conn<'_>, repo_id: Option<&str>) -> code_sql::Result<()> {
        c.execute(
            "UPDATE work_items SET state = 'open', claimed_by = NULL, lease_until = NULL, updated_at = ?1 \
             WHERE state = 'claimed' AND lease_until < ?1 AND (CAST(?2 AS TEXT) IS NULL OR repo_id = ?2)",
            params![now(), repo_id],
        )?;
        Ok(())
    }

    /// Before reading work items: leases that ran out are expired, so an expired lease reads
    /// as open. A read looks for one first; only then is it a write.
    fn expire_leases(&self, repo_id: Option<&str>) -> Result<()> {
        let expired = self
            .sql
            .query_row(
                "SELECT 1 FROM work_items WHERE state = 'claimed' AND lease_until < ?1 AND (CAST(?2 AS TEXT) IS NULL OR repo_id = ?2) LIMIT 1",
                params![now(), repo_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if expired {
            self.write(|c| Self::expire_in(c, repo_id))?;
        }
        Ok(())
    }

    pub fn insert_work(&self, id: &str, repo_id: &str, w: &NewWork) -> Result<()> {
        let labels = json(&w.labels);
        self.write(|c| {
            let number = Self::next_number(c, repo_id)?;
            let t = now();
            c.execute(
                "INSERT INTO work_items (id, repo_id, number, title, body, labels, priority, state, source, github_url, \
                 created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![id, repo_id, number, w.title, w.body, labels, w.priority, w.state.as_str(), w.source, w.github_url, t, t],
            )?;
            Ok(())
        })
    }

    fn with_changes(&self, mut w: WorkItem) -> Result<WorkItem> {
        w.changes = self.changes_for_work(&w.id)?;
        Ok(w)
    }

    pub fn work(&self, id: &str) -> Result<Option<WorkItem>> {
        self.expire_leases(None)?;
        let row = self.sql.query_row(&format!("SELECT {WORK_COLS} FROM work_items WHERE id = ?"), [id], work_row).optional()?;
        row.map(|w| self.with_changes(w)).transpose()
    }

    pub fn work_by_number(&self, repo_id: &str, number: u64) -> Result<Option<WorkItem>> {
        self.expire_leases(Some(repo_id))?;
        let row = self
            .sql
            .query_row(&format!("SELECT {WORK_COLS} FROM work_items WHERE repo_id = ? AND number = ?"), params![repo_id, number as i64], work_row)
            .optional()?;
        row.map(|w| self.with_changes(w)).transpose()
    }

    pub fn work_items(&self, repo_id: &str, state: Option<WorkState>, label: Option<&str>, limit: usize) -> Result<Vec<WorkItem>> {
        self.expire_leases(Some(repo_id))?;
        let has_label = match self.sql.backend() {
            Backend::Sqlite => "EXISTS (SELECT 1 FROM json_each(labels) WHERE value = ?3)",
            Backend::Postgres => "EXISTS (SELECT 1 FROM json_array_elements_text(labels::json) AS l(value) WHERE l.value = ?3)",
        };
        let rows = self.sql.query(
            &format!(
                "SELECT {WORK_COLS} FROM work_items WHERE repo_id = ?1 AND (CAST(?2 AS TEXT) IS NULL OR state = ?2) \
                 AND (CAST(?3 AS TEXT) IS NULL OR {has_label}) \
                 ORDER BY CASE state WHEN 'claimed' THEN 0 WHEN 'open' THEN 1 WHEN 'review' THEN 2 ELSE 3 END, priority, number \
                 LIMIT ?4"
            ),
            params![repo_id, state.map(|s| s.as_str()), label, crate::db::limit(limit)],
            work_row,
        )?;
        rows.into_iter().map(|w| self.with_changes(w)).collect()
    }

    pub fn count_work(&self, repo_id: &str, states: &[&str]) -> Result<u64> {
        self.expire_leases(Some(repo_id))?;
        let list = states.iter().map(|s| format!("'{s}'")).collect::<Vec<_>>().join(",");
        let n: i64 = self.sql.query_row(&format!("SELECT COUNT(*) FROM work_items WHERE repo_id = ? AND state IN ({list})"), [repo_id], |r| r.get(0))?;
        Ok(n as u64)
    }

    pub fn work_by_source(&self, repo_id: &str, source: &str) -> Result<Option<WorkItem>> {
        let row = self
            .sql
            .query_row(&format!("SELECT {WORK_COLS} FROM work_items WHERE repo_id = ? AND source = ?"), params![repo_id, source], work_row)
            .optional()?;
        row.map(|w| self.with_changes(w)).transpose()
    }

    /// Lease the highest-priority (then oldest) open item that has every label in `labels`.
    /// One write, so of agents claiming at once (on any node) each item goes to exactly one.
    pub fn claim(&self, repo_id: &str, agent: &str, labels: &[String], lease_s: i64) -> Result<Option<String>> {
        self.write(|c| {
            Self::expire_in(c, Some(repo_id))?;
            let candidates: Vec<(String, String)> = c.query(
                "SELECT id, labels FROM work_items WHERE repo_id = ? AND state = 'open' ORDER BY priority, number LIMIT 1000",
                [repo_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let pick = candidates.into_iter().find(|(_, l)| {
                let have: Vec<String> = serde_json::from_str(l).unwrap_or_default();
                labels.iter().all(|want| have.iter().any(|h| h.eq_ignore_ascii_case(want)))
            });
            let Some((id, _)) = pick else { return Ok(None) };
            let t = now();
            let n = c.execute(
                "UPDATE work_items SET state = 'claimed', claimed_by = ?, lease_until = ?, lease_s = ?, updated_at = ? WHERE id = ? AND state = 'open'",
                params![agent, t + lease_s, lease_s, t, id],
            )?;
            Ok((n == 1).then_some(id))
        })
    }

    /// The lease length the item was claimed with.
    pub fn lease_len(&self, id: &str) -> Result<Option<i64>> {
        Ok(self.sql.query_row("SELECT lease_s FROM work_items WHERE id = ?", [id], |r| r.get(0)).optional()?.flatten())
    }

    pub fn set_lease_len(&self, id: &str, lease_s: i64) -> Result<()> {
        self.sql.execute("UPDATE work_items SET lease_s = ? WHERE id = ?", params![lease_s, id])?;
        Ok(())
    }

    pub fn update_work(&self, w: &WorkItem) -> Result<()> {
        self.sql.execute(
            "UPDATE work_items SET title = ?, body = ?, labels = ?, priority = ?, state = ?, claimed_by = ?, lease_until = ?, \
             github_url = ?, updated_at = ? WHERE id = ?",
            params![w.title, w.body, json(&w.labels), w.priority, w.state.as_str(), w.claimed_by, w.lease_until, w.github_url, now(), w.id],
        )?;
        Ok(())
    }

    pub fn set_import(&self, repo_id: &str, import: Option<&Value>) -> Result<()> {
        self.sql.execute("UPDATE repos SET import = ?, updated_at = ? WHERE id = ?", params![import.map(json), now(), repo_id])?;
        Ok(())
    }

    // ── change flags ──

    pub fn update_change_meta(&self, id: &str, title: Option<&str>, body: Option<&str>, draft: Option<bool>) -> Result<()> {
        self.sql.execute(
            "UPDATE changes SET title = COALESCE(?, title), body = COALESCE(?, body), draft = COALESCE(?, draft), updated_at = ? WHERE id = ?",
            params![title, body, draft, now(), id],
        )?;
        Ok(())
    }

    /// A writer approved running `head` of an untrusted change.
    pub fn approve_run(&self, id: &str, head: &str) -> Result<()> {
        self.sql.execute("UPDATE changes SET run_approved_head = ?, updated_at = ? WHERE id = ?", params![head, now(), id])?;
        Ok(())
    }

    // ── merge queue ──

    /// Queue a change at the back of its lane. An already active entry is returned as it is.
    /// One write: the check, the position (`MAX(seq) + 1`) and the insert are atomic with other
    /// nodes enqueueing.
    pub fn enqueue(&self, id: &str, repo_id: &str, change_id: &str, lane: Lane, by: Option<&str>) -> Result<EntryRow> {
        self.write(|c| {
            let existing = c
                .query_row(
                    &format!("SELECT {ENTRY_COLS} FROM queue_entries WHERE change_id = ? AND state NOT IN ('ejected', 'landed')"),
                    [change_id],
                    entry_row,
                )
                .optional()?;
            if let Some(e) = existing {
                return Ok(e);
            }
            let t = now();
            c.execute(
                "INSERT INTO queue_entries (id, change_id, repo_id, seq, lane, state, enqueued_by, enqueued_at, updated_at) \
                 VALUES (?, ?, ?, (SELECT COALESCE(MAX(seq), 0) + 1 FROM queue_entries), ?, 'waiting', ?, ?, ?)",
                params![id, change_id, repo_id, lane.as_str(), by, t, t],
            )?;
            c.query_row(&format!("SELECT {ENTRY_COLS} FROM queue_entries WHERE id = ?"), [id], entry_row)
        })
    }

    /// The change's latest entry (the active one, or the last one that finished).
    pub fn entry_for_change(&self, change_id: &str) -> Result<Option<EntryRow>> {
        Ok(self
            .sql
            .query_row(
                &format!(
                    "SELECT {ENTRY_COLS} FROM queue_entries WHERE change_id = ? \
                     ORDER BY (state NOT IN ('ejected', 'landed')) DESC, seq DESC LIMIT 1"
                ),
                [change_id],
                entry_row,
            )
            .optional()?)
    }

    /// Active entries in queue order.
    pub fn active_entries(&self, repo_id: &str) -> Result<Vec<EntryRow>> {
        Ok(self.sql.query(
            &format!("SELECT {ENTRY_COLS} FROM queue_entries WHERE repo_id = ? AND state NOT IN ('ejected', 'landed') ORDER BY {QUEUE_ORDER}"),
            [repo_id],
            entry_row,
        )?)
    }

    /// 1-based place of an active entry in queue order.
    pub fn position(&self, e: &EntryRow) -> Result<u64> {
        let rank = if e.state == EntryState::Waiting { 2 } else { 0 } + if e.lane == Lane::Urgent { 0 } else { 1 };
        let ahead: i64 = self.sql.query_row(
            &format!(
                "SELECT COUNT(*) FROM queue_entries WHERE repo_id = ?1 AND state NOT IN ('ejected', 'landed') AND \
                 ({QUEUE_RANK} < ?2 OR ({QUEUE_RANK} = ?2 AND seq < ?3))"
            ),
            params![e.repo_id, rank, e.seq],
            |r| r.get(0),
        )?;
        Ok(ahead as u64 + 1)
    }

    /// An open (or queued) change whose head is `head`.
    pub fn change_by_head(&self, repo_id: &str, head: &str) -> Result<Option<Change>> {
        let row = self
            .sql
            .query_row(
                &format!("SELECT {CHANGE_COLS} FROM changes WHERE repo_id = ? AND head = ? AND state IN ('open', 'queued') ORDER BY number DESC LIMIT 1"),
                params![repo_id, head],
                change_row,
            )
            .optional()?;
        row.map(|c| self.with_checks(c)).transpose()
    }

    /// The first `limit` waiting entries, in queue order.
    pub fn waiting_entries(&self, repo_id: &str, limit: usize) -> Result<Vec<EntryRow>> {
        Ok(self.sql.query(
            &format!("SELECT {ENTRY_COLS} FROM queue_entries WHERE repo_id = ? AND state = 'waiting' ORDER BY {QUEUE_ORDER} LIMIT ?"),
            params![repo_id, crate::db::limit(limit)],
            entry_row,
        )?)
    }

    /// Entries that left the queue most recently.
    pub fn finished_entries(&self, repo_id: &str, limit: usize) -> Result<Vec<EntryRow>> {
        Ok(self.sql.query(
            &format!(
                "SELECT {ENTRY_COLS} FROM queue_entries WHERE repo_id = ? AND state IN ('ejected', 'landed') \
                 ORDER BY finished_at DESC NULLS LAST, seq DESC LIMIT ?"
            ),
            params![repo_id, crate::db::limit(limit)],
            entry_row,
        )?)
    }

    /// Changes in a repo's merge queue.
    pub fn queue_depth(&self, repo_id: &str) -> Result<u64> {
        let n: i64 =
            self.sql.query_row("SELECT COUNT(*) FROM queue_entries WHERE repo_id = ? AND state NOT IN ('ejected', 'landed')", [repo_id], |r| r.get(0))?;
        Ok(n as u64)
    }

    /// Repos with something in their queue.
    pub fn queued_repos(&self) -> Result<Vec<String>> {
        Ok(self.sql.query("SELECT DISTINCT repo_id FROM queue_entries WHERE state NOT IN ('ejected', 'landed')", (), |r| r.get(0))?)
    }

    /// Move an active entry to `state` (with its batch, and the changeset it lands as).
    pub fn set_entry(&self, id: &str, state: EntryState, batch_id: Option<&str>, landed_as: Option<&str>) -> Result<()> {
        self.sql.execute(
            "UPDATE queue_entries SET state = ?, batch_id = ?, landed_as = ?, updated_at = ? \
             WHERE id = ? AND state NOT IN ('ejected', 'landed')",
            params![state.as_str(), batch_id, landed_as, now(), id],
        )?;
        Ok(())
    }

    /// Take an entry out of the queue for good. `false` if it already left.
    pub fn finish_entry(&self, id: &str, state: EntryState, error: Option<&str>, landed_as: Option<&str>) -> Result<bool> {
        self.finish_entry_with(id, state, error, None, landed_as)
    }

    /// [`finish_entry`](Self::finish_entry), with the output (log tail) behind `error`.
    pub fn finish_entry_with(&self, id: &str, state: EntryState, error: Option<&str>, output: Option<&str>, landed_as: Option<&str>) -> Result<bool> {
        let t = now();
        Ok(self.sql.execute(
            "UPDATE queue_entries SET state = ?, error = ?, output = ?, landed_as = COALESCE(?, landed_as), updated_at = ?, finished_at = ? \
             WHERE id = ? AND state NOT IN ('ejected', 'landed')",
            params![state.as_str(), error, output, landed_as, t, t, id],
        )? == 1)
    }

    /// Move an active entry to another lane, keeping its place in time (`seq`, `enqueued_at`).
    pub fn set_entry_lane(&self, change_id: &str, lane: Lane) -> Result<bool> {
        Ok(self.sql.execute(
            "UPDATE queue_entries SET lane = ?, updated_at = ? WHERE change_id = ? AND state NOT IN ('ejected', 'landed')",
            params![lane.as_str(), now(), change_id],
        )? == 1)
    }

    /// Remove a change's active entry (dequeue). Returns it, if there was one.
    pub fn remove_entry(&self, change_id: &str) -> Result<Option<EntryRow>> {
        self.write(|c| {
            let e = c
                .query_row(
                    &format!("SELECT {ENTRY_COLS} FROM queue_entries WHERE change_id = ? AND state NOT IN ('ejected', 'landed')"),
                    [change_id],
                    entry_row,
                )
                .optional()?;
            if let Some(e) = &e {
                c.execute("DELETE FROM queue_entries WHERE id = ?", [&e.id])?;
            }
            Ok(e)
        })
    }

    /// `(landed in the last hour, their average wait in seconds)`.
    pub fn queue_throughput(&self, repo_id: &str) -> Result<(u64, f64)> {
        let since = now() - 3600;
        Ok(self.sql.query_row(
            "SELECT COUNT(*), COALESCE(AVG(finished_at - enqueued_at), 0) FROM queue_entries \
             WHERE repo_id = ? AND state = 'landed' AND finished_at >= ?",
            params![repo_id, since],
            |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, f64>(1)?)),
        )?)
    }

    pub fn repo_queue(&self, repo_id: &str) -> Result<RepoQueue> {
        Ok(self
            .sql
            .query_row("SELECT paused, batch_size, last_green FROM repo_queue WHERE repo_id = ?", [repo_id], |r| {
                Ok(RepoQueue {
                    paused: r.get::<_, i64>(0)? != 0,
                    batch_size: r.get::<_, Option<i64>>(1)?.map(|n| n.max(1) as usize),
                    last_green: r.get(2)?,
                })
            })
            .optional()?
            .unwrap_or_default())
    }

    pub fn set_paused(&self, repo_id: &str, paused: bool) -> Result<()> {
        self.sql.execute(
            "INSERT INTO repo_queue (repo_id, paused, updated_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT (repo_id) DO UPDATE SET paused = excluded.paused, updated_at = excluded.updated_at",
            params![repo_id, paused, now()],
        )?;
        Ok(())
    }

    pub fn set_batch_size(&self, repo_id: &str, size: usize) -> Result<()> {
        self.sql.execute(
            "INSERT INTO repo_queue (repo_id, batch_size, updated_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT (repo_id) DO UPDATE SET batch_size = excluded.batch_size, updated_at = excluded.updated_at",
            params![repo_id, size as i64, now()],
        )?;
        Ok(())
    }

    pub fn set_last_green(&self, repo_id: &str, cs: &str) -> Result<()> {
        self.sql.execute(
            "INSERT INTO repo_queue (repo_id, last_green, updated_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT (repo_id) DO UPDATE SET last_green = excluded.last_green, updated_at = excluded.updated_at",
            params![repo_id, cs, now()],
        )?;
        Ok(())
    }

    // ── audit ──

    pub fn audit(&self, repo_id: &str, actor: &str, action: &str, detail: &Value) -> Result<()> {
        self.sql.execute(
            "INSERT INTO audit (repo_id, at, actor, action, detail) VALUES (?, ?, ?, ?, ?)",
            params![repo_id, now(), actor, action, json(detail)],
        )?;
        Ok(())
    }

    /// Newest first.
    pub fn audit_log(&self, repo_id: &str, limit: usize) -> Result<Vec<Value>> {
        Ok(self.sql.query(
            "SELECT id, at, actor, action, detail FROM audit WHERE repo_id = ? ORDER BY id DESC LIMIT ?",
            params![repo_id, crate::db::limit(limit)],
            |r| {
                Ok(json!({
                    "id": r.get::<_, i64>(0)?,
                    "at": r.get::<_, i64>(1)?,
                    "actor": r.get::<_, String>(2)?,
                    "action": r.get::<_, String>(3)?,
                    "detail": serde_json::from_str::<Value>(&r.get::<_, String>(4)?).unwrap_or(Value::Null),
                }))
            },
        )?)
    }
}

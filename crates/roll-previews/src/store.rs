//! The previews database (docs/previews.md, "Data model"). Every read-then-write is one
//! [`Db::write`] closure, so nodes sharing the system database need no leases for it; write
//! closures only touch the database (they may re-run).

use crate::cost::{Cost, HOUR_MS, MONTH_HOURS, Rates};
use crate::settings::Settings;
use crate::{Activity, Desired, Error, Kind, Mode, Preview, Resource, ResourceState, Result, Service, ServiceState, State};
use code_sql::{Conn, Db, OptionalExtension, Target, params};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS settings (
    project_id TEXT PRIMARY KEY,
    json TEXT NOT NULL,
    updated_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS previews (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    slug TEXT NOT NULL UNIQUE,
    repo TEXT NOT NULL,
    change_id TEXT UNIQUE,
    branch TEXT,
    number INTEGER,
    title TEXT NOT NULL,
    github_pr INTEGER,
    head TEXT NOT NULL,
    trusted INTEGER NOT NULL,
    desired TEXT NOT NULL,
    state TEXT NOT NULL,
    error TEXT,
    attempts INTEGER NOT NULL,
    next_at INTEGER NOT NULL,
    asleep INTEGER NOT NULL,
    reset_requested TEXT,
    last_reset_at INTEGER,
    last_active_at INTEGER NOT NULL,
    expires_at INTEGER,
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS previews_project ON previews (project_id);
CREATE TABLE IF NOT EXISTS resources (
    preview_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    parent TEXT NOT NULL,
    mode TEXT NOT NULL,
    ref TEXT,
    state TEXT NOT NULL,
    error TEXT,
    bytes INTEGER NOT NULL,
    usd_month REAL NOT NULL,
    usd_hour REAL NOT NULL,
    spent_usd REAL NOT NULL,
    kept TEXT,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (preview_id, kind, parent)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS resources_ref ON resources (kind, ref);
CREATE TABLE IF NOT EXISTS services (
    preview_id TEXT NOT NULL,
    name TEXT NOT NULL,
    repo TEXT NOT NULL,
    host TEXT NOT NULL UNIQUE,
    url TEXT NOT NULL,
    revision TEXT,
    deployed TEXT,
    state TEXT NOT NULL,
    error TEXT,
    memory_mb INTEGER NOT NULL,
    active_ms INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (preview_id, name)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS spend (
    project_id TEXT NOT NULL,
    month INTEGER NOT NULL,
    usd REAL NOT NULL,
    PRIMARY KEY (project_id, month)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS pinned (
    preview_id TEXT NOT NULL,
    service TEXT NOT NULL,
    revision TEXT NOT NULL,
    host TEXT NOT NULL UNIQUE,
    url TEXT NOT NULL,
    at INTEGER NOT NULL,
    PRIMARY KEY (preview_id, service, revision)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS dismissed (
    change_id TEXT PRIMARY KEY,
    head TEXT NOT NULL,
    at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS activity (
    preview_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    at INTEGER NOT NULL,
    message TEXT NOT NULL,
    PRIMARY KEY (preview_id, seq)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS keys (
    name TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS screenshots (
    preview_id TEXT PRIMARY KEY,
    head TEXT NOT NULL,
    png TEXT NOT NULL,
    at INTEGER NOT NULL,
    tried TEXT
);
CREATE TABLE IF NOT EXISTS budget_alerts (
    project_id TEXT NOT NULL,
    month INTEGER NOT NULL,
    max_usd REAL NOT NULL,
    level INTEGER NOT NULL,
    at INTEGER NOT NULL,
    PRIMARY KEY (project_id, month, max_usd, level)
);
CREATE TABLE IF NOT EXISTS bypass (
    project_id TEXT PRIMARY KEY,
    hash TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
";

/// How long a screenshot link (in a pull request comment) works after the screenshot was taken.
pub const SCREENSHOT_LINK_MS: i64 = 30 * 24 * 3_600_000;

/// The newest activity lines kept per preview.
pub const ACTIVITY_KEEP: i64 = 200;
/// The longest slug: leaves room for a service name in a 63-character host label.
pub const SLUG_MAX: usize = 40;

/// Who may open a preview's services (`runtime`'s gate).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Access {
    /// The link's token.
    pub token: String,
    /// Whether the link is needed (`access: link`), or anyone may (`public`).
    pub required: bool,
    /// The project's bypass secret's SHA-256 (hex), for automation (`x-roll-preview-bypass`);
    /// never for a fork's (`trusted: false`) preview.
    pub bypass: Option<String>,
}

/// The SHA-256 (hex) a bypass secret is kept as.
pub fn bypass_hash(secret: &str) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(secret.as_bytes()))
}

/// Which preview a branched resource belongs to (for scoped tokens: feature/roll-mcp).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Environment {
    pub preview_id: String,
    pub project_id: String,
}

/// A preview as the engine works on it: the row, without its lists.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub id: String,
    pub project_id: String,
    pub slug: String,
    pub repo: String,
    pub change_id: Option<String>,
    pub branch: Option<String>,
    pub number: Option<i64>,
    pub title: String,
    pub github_pr: Option<i64>,
    pub head: String,
    pub trusted: bool,
    pub desired: Desired,
    pub state: State,
    pub error: Option<String>,
    pub attempts: i64,
    pub next_at: i64,
    pub asleep: bool,
    /// Resources to re-branch (`"<kind>:<parent>"`), or `["*"]` for all.
    pub reset_requested: Option<Vec<String>>,
    pub last_reset_at: Option<i64>,
    pub last_active_at: i64,
    pub expires_at: Option<i64>,
    pub created_by: String,
    pub created_at: i64,
    pub updated_at: i64,
    /// Bumped by every write that asks for a converge (`next_at = 0`): a converge settles a
    /// row only if nothing asked for another one meanwhile.
    pub generation: i64,
    /// The change's earlier head its own repo's services keep running, because every push since
    /// touched only paths the path filters ignore (`None`: they run `head`).
    pub runs: Option<String>,
}

impl Row {
    /// The head the change's own repo's services, release, seed, schema and screenshot are of.
    pub fn build(&self) -> &str {
        self.runs.as_deref().unwrap_or(&self.head)
    }
}

/// What a new preview is made from.
#[derive(Clone, Debug)]
pub struct New {
    pub project_id: String,
    /// The slug's first part: `pr482`, `c17`, a branch name.
    pub key: String,
    pub project_name: String,
    pub repo: String,
    pub change_id: Option<String>,
    pub branch: Option<String>,
    pub number: Option<i64>,
    pub title: String,
    pub github_pr: Option<i64>,
    pub head: String,
    pub trusted: bool,
    pub state: State,
    pub expires_at: Option<i64>,
    pub created_by: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResourceRow {
    pub kind: Kind,
    pub parent: String,
    pub mode: Mode,
    pub r#ref: Option<String>,
    pub state: ResourceState,
    pub error: Option<String>,
    pub bytes: i64,
    pub usd_month: f64,
    pub usd_hour: f64,
    pub spent_usd: f64,
    /// Credentials the product gave out once ([`crate::ports::Branched::kept`]).
    pub kept: Option<String>,
}

pub struct Store {
    /// The screenshot links' key, once read (`signing_key`).
    signing: std::sync::OnceLock<Vec<u8>>,
    db: Db,
    rates: Rates,
}

const COLUMNS: &str = "id, project_id, slug, repo, change_id, branch, number, title, github_pr, head, trusted, desired, state, error, attempts, \
     next_at, asleep, reset_requested, last_reset_at, last_active_at, expires_at, created_by, created_at, updated_at, generation, runs";

fn row(r: &code_sql::Row<'_>) -> code_sql::Result<Row> {
    let desired: String = r.get(11)?;
    let state: String = r.get(12)?;
    let reset: Option<String> = r.get(17)?;
    Ok(Row {
        id: r.get(0)?,
        project_id: r.get(1)?,
        slug: r.get(2)?,
        repo: r.get(3)?,
        change_id: r.get(4)?,
        branch: r.get(5)?,
        number: r.get(6)?,
        title: r.get(7)?,
        github_pr: r.get(8)?,
        head: r.get(9)?,
        trusted: r.get::<_, i64>(10)? != 0,
        desired: if desired == "deleted" { Desired::Deleted } else { Desired::Active },
        state: State::parse(&state).map_err(|e| code_sql::Error::Conversion(e.to_string()))?,
        error: r.get(13)?,
        attempts: r.get(14)?,
        next_at: r.get(15)?,
        asleep: r.get::<_, i64>(16)? != 0,
        reset_requested: reset.and_then(|s| serde_json::from_str(&s).ok()),
        last_reset_at: r.get(18)?,
        last_active_at: r.get(19)?,
        expires_at: r.get(20)?,
        created_by: r.get(21)?,
        created_at: r.get(22)?,
        updated_at: r.get(23)?,
        generation: r.get(24)?,
        runs: r.get(25)?,
    })
}

fn resource_row(r: &code_sql::Row<'_>) -> code_sql::Result<ResourceRow> {
    let kind: String = r.get(0)?;
    let mode: String = r.get(2)?;
    let state: String = r.get(4)?;
    Ok(ResourceRow {
        kind: Kind::parse(&kind).map_err(|e| code_sql::Error::Conversion(e.to_string()))?,
        parent: r.get(1)?,
        mode: Mode::parse(&mode),
        r#ref: r.get(3)?,
        state: ResourceState::parse(&state),
        error: r.get(5)?,
        bytes: r.get(6)?,
        usd_month: r.get(7)?,
        usd_hour: r.get(8)?,
        spent_usd: r.get(9)?,
        kept: r.get(10)?,
    })
}

const RESOURCE_COLUMNS: &str = "kind, parent, mode, ref, state, error, bytes, usd_month, usd_hour, spent_usd, kept";

fn service_row(r: &code_sql::Row<'_>) -> code_sql::Result<(Service, i64)> {
    let state: String = r.get(6)?;
    Ok((
        Service {
            name: r.get(0)?,
            repo: r.get(1)?,
            host: r.get(2)?,
            url: r.get(3)?,
            revision: r.get(4)?,
            deployed: r.get(5)?,
            state: ServiceState::parse(&state),
            error: r.get(7)?,
            memory_mb: r.get(8)?,
            share_url: None,
        },
        // Running time counts for the month it was recorded in.
        if r.get::<_, i64>(10)? == month_start(code_sql::now_ms()) { r.get(9)? } else { 0 },
    ))
}

const SERVICE_COLUMNS: &str = "name, repo, host, url, revision, deployed, state, error, memory_mb, active_ms, active_month";

fn json_list(v: &Option<Vec<String>>) -> Option<String> {
    v.as_ref().map(|l| serde_json::to_string(l).expect("JSON"))
}

impl Store {
    pub fn open(target: &Target, pg: code_sql::PgOptions) -> Result<Store> {
        let db = match target {
            // Tests run on Postgres too (`ROLL_CODE_TEST_DATABASE`), as a cluster's store does.
            Target::Sqlite(_) if std::env::var(code_sql::TEST_DATABASE_ENV).is_ok_and(|u| !u.is_empty()) => Db::open_or_test(target, "previews")?,
            Target::Sqlite(path) => Db::open_sqlite(path, code_sql::SqliteOptions { readers: 2, cache_kib: 2 << 10 })?,
            Target::Postgres { url, schema } => Db::connect(url, schema, pg)?,
        };
        Store::new(db)
    }

    pub fn new(db: Db) -> Result<Store> {
        db.ensure_schema(SCHEMA)?;
        if !db.has_column("previews", "token")? {
            db.add_column("previews", "token", "TEXT")?;
        }
        if !db.has_column("previews", "generation")? {
            db.add_column("previews", "generation", "INTEGER NOT NULL DEFAULT 0")?;
        }
        // Rows from before access tokens: each gets one (a missing token never lets a request
        // through).
        let untokened: Vec<String> = db.read(|c| c.query("SELECT id FROM previews WHERE token IS NULL OR token = ''", (), |r| r.get(0)))?;
        for id in untokened {
            let t = new_token();
            db.write(|c| c.execute("UPDATE previews SET token = ? WHERE id = ? AND (token IS NULL OR token = '')", params![t, id]).map(drop))?;
        }
        if !db.has_column("services", "active_month")? {
            db.add_column("services", "active_month", "INTEGER NOT NULL DEFAULT 0")?;
        }
        for col in ["config", "healthy"] {
            if !db.has_column("services", col)? {
                db.add_column("services", col, "TEXT")?;
            }
        }
        if !db.has_column("previews", "idled")? {
            db.add_column("previews", "idled", "INTEGER NOT NULL DEFAULT 0")?;
        }
        if !db.has_column("previews", "seed_needed")? {
            db.add_column("previews", "seed_needed", "INTEGER NOT NULL DEFAULT 1")?;
        }
        for (col, ty) in [("tried", "TEXT"), ("prev_head", "TEXT"), ("prev_png", "TEXT"), ("change_bp", "INTEGER"), ("speed", "TEXT"), ("prev_speed", "TEXT")] {
            if !db.has_column("screenshots", col)? {
                db.add_column("screenshots", col, ty)?;
            }
        }
        if !db.has_column("resources", "mask_digest")? {
            db.add_column("resources", "mask_digest", "TEXT")?;
        }
        if !db.has_column("previews", "vars")? {
            db.add_column("previews", "vars", "TEXT")?;
        }
        for col in ["kept_until", "goes_at"] {
            if !db.has_column("previews", col)? {
                db.add_column("previews", col, "INTEGER")?;
            }
        }
        for col in ["released", "schema_head", "schema_changes", "runs"] {
            if !db.has_column("previews", col)? {
                db.add_column("previews", col, "TEXT")?;
            }
        }
        Ok(Store { db, rates: Rates::default(), signing: std::sync::OnceLock::new() })
    }

    pub fn now_ms(&self) -> Result<i64> {
        Ok(self.db.now_ms()?)
    }

    // ── settings ──

    pub fn settings(&self, project: &str) -> Result<Settings> {
        let raw: Option<String> = self.db.read(|c| c.query_row("SELECT json FROM settings WHERE project_id = ?", [project], |r| r.get(0)).optional())?;
        // Only checked settings are stored; a row that doesn't parse (an older version's) is the
        // defaults with previews off, never a guess.
        Ok(raw.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default())
    }

    /// Projects with previews on.
    pub fn enabled_projects(&self) -> Result<Vec<(String, Settings)>> {
        let rows: Vec<(String, String)> = self.db.read(|c| c.query("SELECT project_id, json FROM settings ORDER BY project_id", (), |r| Ok((r.get(0)?, r.get(1)?))))?;
        Ok(rows.into_iter().filter_map(|(p, j)| serde_json::from_str::<Settings>(&j).ok().filter(|s| s.enabled).map(|s| (p, s))).collect())
    }

    /// Projects whose stored rules parse and say previews are off (rules that don't parse,
    /// a newer version's, say, are neither on nor off: nothing is deleted for them).
    pub fn disabled_projects(&self) -> Result<Vec<String>> {
        let rows: Vec<(String, String)> = self.db.read(|c| c.query("SELECT project_id, json FROM settings ORDER BY project_id", (), |r| Ok((r.get(0)?, r.get(1)?))))?;
        Ok(rows.into_iter().filter(|(_, j)| serde_json::from_str::<Settings>(j).is_ok_and(|s| !s.enabled)).map(|(p, _)| p).collect())
    }

    /// What deleted previews of `project` cost this month (live ones count their own).
    pub fn spent(&self, project: &str, month: i64) -> Result<f64> {
        Ok(self.db.read(|c| c.query_row("SELECT usd FROM spend WHERE project_id = ? AND month = ?", params![project, month], |r| r.get::<_, f64>(0)).optional())?.unwrap_or(0.0))
    }

    pub fn add_spend(&self, project: &str, month: i64, usd: f64) -> Result<()> {
        self.db.write(|c| {
            c.execute(
                "INSERT INTO spend (project_id, month, usd) VALUES (?, ?, ?) ON CONFLICT (project_id, month) DO UPDATE SET usd = spend.usd + excluded.usd",
                params![project, month, usd],
            )?;
            Ok::<_, Error>(())
        })
    }

    /// The highest budget alert level (percent) sent for `project` this `month` against a budget
    /// of `max_usd`, if any (a new budget is alerted on afresh).
    pub fn alerted(&self, project: &str, month: i64, max_usd: f64) -> Result<Option<i64>> {
        Ok(self.db.read(|c| c.query_row("SELECT MAX(level) FROM budget_alerts WHERE project_id = ? AND month = ? AND max_usd = ?", params![project, month, max_usd], |r| r.get(0)))?)
    }

    /// Record that the alert at `level` was queued (after it was: a crash repeats one, never
    /// loses one).
    pub fn set_alerted(&self, project: &str, month: i64, max_usd: f64, level: i64) -> Result<()> {
        let now = self.now_ms()?;
        self.db.write(|c| {
            c.execute(
                "INSERT INTO budget_alerts (project_id, month, max_usd, level, at) VALUES (?, ?, ?, ?, ?) ON CONFLICT DO NOTHING",
                params![project, month, max_usd, level, now],
            )?;
            Ok::<_, Error>(())
        })
    }

    /// A service's deploy fingerprint (what it was deployed with: variables, access).
    pub fn service_config(&self, id: &str, name: &str) -> Result<Option<String>> {
        Ok(self.db.read(|c| c.query_row("SELECT config FROM services WHERE preview_id = ? AND name = ?", [id, name], |r| r.get::<_, Option<String>>(0)).optional())?.flatten())
    }

    /// The revision a service last passed its health check at.
    pub fn healthy(&self, id: &str, name: &str) -> Result<Option<String>> {
        Ok(self.db.read(|c| c.query_row("SELECT healthy FROM services WHERE preview_id = ? AND name = ?", [id, name], |r| r.get::<_, Option<String>>(0)).optional())?.flatten())
    }

    pub fn set_healthy(&self, id: &str, name: &str, revision: &str) -> Result<()> {
        self.db.write(|c| {
            c.execute("UPDATE services SET healthy = ? WHERE preview_id = ? AND name = ?", params![revision, id, name])?;
            Ok::<_, Error>(())
        })
    }

    pub fn set_service_config(&self, id: &str, name: &str, config: &str) -> Result<()> {
        self.db.write(|c| {
            c.execute("UPDATE services SET config = ? WHERE preview_id = ? AND name = ?", params![config, id, name])?;
            Ok::<_, Error>(())
        })
    }

    pub fn put_settings(&self, project: &str, s: &Settings) -> Result<Settings> {
        s.check()?;
        let json = serde_json::to_string(s).expect("JSON");
        let now = self.now_ms()?;
        self.db.write(|c| {
            c.execute(
                "INSERT INTO settings (project_id, json, updated_at) VALUES (?, ?, ?) ON CONFLICT (project_id) DO UPDATE SET json = excluded.json, updated_at = excluded.updated_at",
                params![project, json, now],
            )?;
            Ok::<_, Error>(())
        })?;
        Ok(s.clone())
    }

    pub fn delete_settings(&self, project: &str) -> Result<()> {
        self.db.write(|c| {
            c.execute("DELETE FROM settings WHERE project_id = ?", [project])?;
            c.execute("DELETE FROM bypass WHERE project_id = ?", [project])?;
            c.execute("DELETE FROM budget_alerts WHERE project_id = ?", [project])?;
            Ok::<_, Error>(())
        })
    }

    // ── previews ──

    /// Make a preview, or return the one the change already has. The slug is `<key>-<project>`
    /// (cut to [`SLUG_MAX`]), with the id's end added when that's taken.
    pub fn create(&self, n: &New) -> Result<(Row, bool)> {
        let id = crate::new_id();
        let now = self.now_ms()?;
        let base = crate::label_part(&format!("{}-{}", n.key, n.project_name), SLUG_MAX);
        let base = if base.is_empty() { crate::label_part(&id, SLUG_MAX) } else { base };
        self.db.write(|c| {
            if let Some(change) = &n.change_id
                && let Some(existing) = c.query_row(&format!("SELECT {COLUMNS} FROM previews WHERE change_id = ?"), [change], row).optional()?
            {
                return Ok((existing, false));
            }
            let mut slug = base.clone();
            if slug_taken(c, &slug)? {
                let tail = &id[id.len() - 4..];
                slug = format!("{}-{tail}", crate::label_part(&base, SLUG_MAX - 5));
                if slug_taken(c, &slug)? {
                    slug = crate::label_part(&id, SLUG_MAX);
                }
            }
            c.execute(
                &format!("INSERT INTO previews ({COLUMNS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'active', ?, NULL, 0, 0, 0, NULL, NULL, ?, ?, ?, ?, ?, 0, NULL)"),
                params![
                    id,
                    n.project_id,
                    slug,
                    n.repo,
                    n.change_id,
                    n.branch,
                    n.number,
                    n.title,
                    n.github_pr,
                    n.head,
                    n.trusted as i64,
                    n.state.as_str(),
                    now,
                    n.expires_at,
                    n.created_by,
                    now,
                    now
                ],
            )?;
            c.execute("UPDATE previews SET token = ? WHERE id = ?", params![new_token(), id])?;
            append(c, &id, now, "preview created")?;
            let r = c.query_row(&format!("SELECT {COLUMNS} FROM previews WHERE id = ?"), [&id], row)?;
            Ok((r, true))
        })
    }

    pub fn row(&self, id: &str) -> Result<Row> {
        self.db
            .read(|c| c.query_row(&format!("SELECT {COLUMNS} FROM previews WHERE id = ?"), [id], row).optional())?
            .ok_or_else(|| Error::NotFound(format!("no preview {id}")))
    }

    pub fn row_by_change(&self, change: &str) -> Result<Option<Row>> {
        Ok(self.db.read(|c| c.query_row(&format!("SELECT {COLUMNS} FROM previews WHERE change_id = ?"), [change], row).optional())?)
    }

    /// Every preview of `project` (all projects with `None`), newest first.
    pub fn rows(&self, project: Option<&str>) -> Result<Vec<Row>> {
        Ok(self.db.read(|c| match project {
            Some(p) => c.query(&format!("SELECT {COLUMNS} FROM previews WHERE project_id = ? ORDER BY created_at DESC, id"), [p], row),
            None => c.query(&format!("SELECT {COLUMNS} FROM previews ORDER BY created_at DESC, id"), (), row),
        })?)
    }

    /// Change a preview's row in one write. `f` gets the row as stored and edits it; the edit is
    /// stored (with `updated_at`) unless it's unchanged. Returns the stored row.
    pub fn update(&self, id: &str, mut f: impl FnMut(&mut Row)) -> Result<Row> {
        let now = self.now_ms()?;
        self.db.write(|c| {
            let old = c.query_row(&format!("SELECT {COLUMNS} FROM previews WHERE id = ?"), [id], row).optional()?;
            let Some(old) = old else { return Err(Error::NotFound(format!("no preview {id}"))) };
            let mut new = old.clone();
            f(&mut new);
            if new == old {
                return Ok(old);
            }
            new.updated_at = now;
            if new.next_at == 0 {
                new.generation = old.generation + 1;
            }
            c.execute(
                "UPDATE previews SET title = ?, head = ?, trusted = ?, github_pr = ?, desired = ?, state = ?, error = ?, attempts = ?, next_at = ?, \
                 asleep = ?, reset_requested = ?, last_reset_at = ?, last_active_at = ?, expires_at = ?, updated_at = ?, generation = ?, runs = ? WHERE id = ?",
                params![
                    new.title,
                    new.head,
                    new.trusted as i64,
                    new.github_pr,
                    if new.desired == Desired::Deleted { "deleted" } else { "active" },
                    new.state.as_str(),
                    new.error,
                    new.attempts,
                    new.next_at,
                    new.asleep as i64,
                    json_list(&new.reset_requested),
                    new.last_reset_at,
                    new.last_active_at,
                    new.expires_at,
                    new.updated_at,
                    new.generation,
                    new.runs,
                    id
                ],
            )?;
            Ok(new)
        })
    }

    /// Note that the preview was used (a request, a push) without a write per request: only
    /// when the stored time is a minute old.
    pub fn touch(&self, id: &str) -> Result<()> {
        let now = self.now_ms()?;
        self.db.write(|c| {
            c.execute("UPDATE previews SET last_active_at = ?, idled = 0 WHERE id = ? AND last_active_at < ?", params![now, id, now - 60_000])?;
            Ok::<_, Error>(())
        })
    }

    /// Remove a preview's rows (after its branches and services are gone).
    pub fn remove(&self, id: &str) -> Result<()> {
        self.db.write(|c| {
            for t in ["resources", "services", "pinned", "activity", "screenshots", "previews"] {
                let col = if t == "previews" { "id" } else { "preview_id" };
                c.execute(&format!("DELETE FROM {t} WHERE {col} = ?"), [id])?;
            }
            Ok::<_, Error>(())
        })
    }

    /// A change's preview was deleted while the change stays open (its TTL ran out, or someone
    /// deleted it): no new one until the change's head moves.
    pub fn dismiss(&self, change: &str, head: &str) -> Result<()> {
        let now = self.now_ms()?;
        self.db.write(|c| {
            c.execute(
                "INSERT INTO dismissed (change_id, head, at) VALUES (?, ?, ?) ON CONFLICT (change_id) DO UPDATE SET head = excluded.head, at = excluded.at",
                params![change, head, now],
            )?;
            Ok::<_, Error>(())
        })
    }

    /// Whether a change's preview was dismissed at this head.
    pub fn dismissed(&self, change: &str, head: &str) -> Result<bool> {
        Ok(self.db.read(|c| c.query_row("SELECT 1 FROM dismissed WHERE change_id = ? AND head = ?", [change, head], |_| Ok(())).optional())?.is_some())
    }

    /// Forget dismissals of changes that are gone (landed, closed or deleted).
    pub fn undismiss(&self, change: &str) -> Result<()> {
        self.db.write(|c| {
            c.execute("DELETE FROM dismissed WHERE change_id = ?", [change])?;
            Ok::<_, Error>(())
        })
    }

    pub fn log(&self, id: &str, message: &str) -> Result<()> {
        let now = self.now_ms()?;
        self.db.write(|c| {
            append(c, id, now, message)?;
            Ok::<_, Error>(())
        })
    }

    pub fn activity(&self, id: &str) -> Result<Vec<Activity>> {
        Ok(self.db.read(|c| {
            c.query("SELECT at, message FROM activity WHERE preview_id = ? ORDER BY seq DESC", [id], |r| Ok(Activity { at: r.get(0)?, message: r.get(1)? }))
        })?)
    }

    // ── resources ──

    pub fn resources(&self, id: &str) -> Result<Vec<ResourceRow>> {
        Ok(self.db.read(|c| c.query(&format!("SELECT {RESOURCE_COLUMNS} FROM resources WHERE preview_id = ? ORDER BY kind, parent"), [id], resource_row))?)
    }

    /// Make the preview's resource rows `wanted`: add the missing ones (pending), keep the rest.
    /// Returns the rows no longer wanted, which the engine deletes before [`Store::drop_resource`].
    pub fn want_resources(&self, id: &str, wanted: &[(Kind, String, Mode)]) -> Result<Vec<ResourceRow>> {
        let now = self.now_ms()?;
        self.db.write(|c| {
            for (kind, parent, mode) in wanted {
                c.execute(
                    "INSERT INTO resources (preview_id, kind, parent, mode, ref, state, error, bytes, usd_month, usd_hour, spent_usd, kept, updated_at) \
                     VALUES (?, ?, ?, ?, NULL, 'pending', NULL, 0, 0, 0, 0, NULL, ?) ON CONFLICT (preview_id, kind, parent) DO NOTHING",
                    params![id, kind.as_str(), parent, mode.as_str(), now],
                )?;
            }
            let all = c.query(&format!("SELECT {RESOURCE_COLUMNS} FROM resources WHERE preview_id = ?"), [id], resource_row)?;
            Ok::<_, Error>(all.into_iter().filter(|r| !wanted.iter().any(|(k, p, _)| *k == r.kind && *p == r.parent)).collect())
        })
    }

    pub fn set_resource(&self, id: &str, kind: Kind, parent: &str, f: impl Fn(&mut ResourceRow)) -> Result<()> {
        let now = self.now_ms()?;
        self.db.write(|c| {
            let old = c
                .query_row(&format!("SELECT {RESOURCE_COLUMNS} FROM resources WHERE preview_id = ? AND kind = ? AND parent = ?"), params![id, kind.as_str(), parent], resource_row)
                .optional()?;
            let Some(old) = old else { return Ok(()) };
            let mut r = old.clone();
            f(&mut r);
            if r != old {
                c.execute(
                    "UPDATE resources SET mode = ?, ref = ?, state = ?, error = ?, bytes = ?, usd_month = ?, usd_hour = ?, spent_usd = ?, kept = ?, updated_at = ? \
                     WHERE preview_id = ? AND kind = ? AND parent = ?",
                    params![r.mode.as_str(), r.r#ref, r.state.as_str(), r.error, r.bytes, r.usd_month, r.usd_hour, r.spent_usd, r.kept, now, id, kind.as_str(), parent],
                )?;
            }
            Ok::<_, Error>(())
        })
    }

    pub fn drop_resource(&self, id: &str, kind: Kind, parent: &str) -> Result<()> {
        self.db.write(|c| {
            c.execute("DELETE FROM resources WHERE preview_id = ? AND kind = ? AND parent = ?", params![id, kind.as_str(), parent])?;
            Ok::<_, Error>(())
        })
    }

    /// Which preview (and project) a branched resource belongs to. For scoped tokens: a rule
    /// limited to the `preview` environment matches exactly these.
    pub fn environment_of(&self, kind: &str, r#ref: &str) -> Result<Option<Environment>> {
        Ok(self.db.read(|c| {
            c.query_row(
                "SELECT p.id, p.project_id FROM resources r JOIN previews p ON p.id = r.preview_id WHERE r.kind = ? AND r.ref = ?",
                [kind, r#ref],
                |r| Ok(Environment { preview_id: r.get(0)?, project_id: r.get(1)? }),
            )
            .optional()
        })?)
    }

    // ── services ──

    pub fn services(&self, id: &str) -> Result<Vec<Service>> {
        Ok(self.services_with_time(id)?.into_iter().map(|(s, _)| s).collect())
    }

    fn services_with_time(&self, id: &str) -> Result<Vec<(Service, i64)>> {
        Ok(self.db.read(|c| c.query(&format!("SELECT {SERVICE_COLUMNS} FROM services WHERE preview_id = ? ORDER BY name"), [id], service_row))?)
    }

    /// Make sure `name` has a row with a host of its own (`<name>-<slug>`, the id's end added
    /// on a clash), and set the revision it should run. Returns the service.
    pub fn want_service(&self, id: &str, name: &str, repo: &str, revision: &str, memory_mb: u32, url: &dyn Fn(&str) -> String) -> Result<Service> {
        let now = self.now_ms()?;
        self.db.write(|c| {
            let slug: String = c.query_row("SELECT slug FROM previews WHERE id = ?", [id], |r| r.get(0))?;
            let existing = c
                .query_row(&format!("SELECT {SERVICE_COLUMNS} FROM services WHERE preview_id = ? AND name = ?"), [id, name], service_row)
                .optional()?;
            if existing.is_none() {
                let mut host = crate::label_part(&format!("{name}-{slug}"), 63);
                if host_taken(c, &host)? {
                    host = format!("{}-{}", crate::label_part(&host, 58), &id[id.len() - 4..]);
                }
                c.execute(
                    "INSERT INTO services (preview_id, name, repo, host, url, revision, deployed, state, error, memory_mb, active_ms, updated_at) \
                     VALUES (?, ?, ?, ?, ?, NULL, NULL, 'pending', NULL, ?, 0, ?)",
                    params![id, name, repo, host, url(&host), memory_mb as i64, now],
                )?;
            }
            c.execute(
                "UPDATE services SET revision = ?, repo = ?, memory_mb = ?, updated_at = ? WHERE preview_id = ? AND name = ?",
                params![revision, repo, memory_mb as i64, now, id, name],
            )?;
            let (s, _) = c.query_row(&format!("SELECT {SERVICE_COLUMNS} FROM services WHERE preview_id = ? AND name = ?"), [id, name], service_row)?;
            Ok::<_, Error>(s)
        })
    }

    pub fn set_service(&self, id: &str, name: &str, state: ServiceState, deployed: Option<&str>, error: Option<&str>) -> Result<()> {
        let now = self.now_ms()?;
        self.db.write(|c| {
            c.execute(
                "UPDATE services SET state = ?, deployed = COALESCE(?, deployed), error = ?, updated_at = ? WHERE preview_id = ? AND name = ?",
                params![state.as_str(), deployed, error, now, id, name],
            )?;
            Ok::<_, Error>(())
        })
    }

    /// A service's URL as its backend chose it (roll apps' own hostnames).
    pub fn set_service_url(&self, id: &str, name: &str, url: &str) -> Result<()> {
        self.db.write(|c| {
            c.execute("UPDATE services SET url = ? WHERE preview_id = ? AND name = ?", params![url, id, name])?;
            Ok::<_, Error>(())
        })
    }

    /// Add `ms` of running time to a service (its share of the month's cost).
    /// Add `ms` of running time to a service, this month's (a new month starts from zero).
    pub fn add_active(&self, id: &str, name: &str, ms: i64) -> Result<()> {
        let month = month_start(self.now_ms()?);
        self.db.write(|c| {
            c.execute(
                "UPDATE services SET active_ms = CASE WHEN active_month = ? THEN active_ms + ? ELSE ? END, active_month = ? WHERE preview_id = ? AND name = ?",
                params![month, ms, ms, month, id, name],
            )?;
            Ok::<_, Error>(())
        })
    }

    pub fn drop_service(&self, id: &str, name: &str) -> Result<()> {
        self.db.write(|c| {
            c.execute("DELETE FROM services WHERE preview_id = ? AND name = ?", [id, name])?;
            Ok::<_, Error>(())
        })
    }

    /// The masking rules a branch was last masked with (`crate::engine::mask_digest`).
    pub fn mask_digest(&self, id: &str, kind: Kind, parent: &str) -> Result<Option<String>> {
        Ok(self
            .db
            .read(|c| c.query_row("SELECT mask_digest FROM resources WHERE preview_id = ? AND kind = ? AND parent = ?", [id, kind.as_str(), parent], |r| r.get(0)).optional())?
            .flatten())
    }

    pub fn set_mask_digest(&self, id: &str, kind: Kind, parent: &str, digest: &str) -> Result<()> {
        self.db.write(|c| {
            c.execute("UPDATE resources SET mask_digest = ? WHERE preview_id = ? AND kind = ? AND parent = ?", [digest, id, kind.as_str(), parent])?;
            Ok::<_, Error>(())
        })
    }

    /// Until when the preview is kept past its TTL (`None`: its rules decide).
    pub fn kept_until(&self, id: &str) -> Result<Option<i64>> {
        Ok(self.db.read(|c| c.query_row("SELECT kept_until FROM previews WHERE id = ?", [id], |r| r.get(0)).optional())?.flatten())
    }

    /// The preview's own variables (`PUT …/env`), over the project's `env`.
    pub fn vars(&self, id: &str) -> Result<std::collections::BTreeMap<String, String>> {
        let raw: Option<String> = self.db.read(|c| c.query_row("SELECT vars FROM previews WHERE id = ?", [id], |r| r.get(0)).optional())?.flatten();
        Ok(raw.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default())
    }

    /// Change the preview's own variables in one write (`f` may run again; it only edits the map).
    pub fn update_vars(&self, id: &str, f: impl Fn(&mut std::collections::BTreeMap<String, String>) -> Result<()>) -> Result<std::collections::BTreeMap<String, String>> {
        self.db.write(|c| {
            let raw: Option<Option<String>> = c.query_row("SELECT vars FROM previews WHERE id = ?", [id], |r| r.get(0)).optional()?;
            let Some(raw) = raw else { return Err(Error::NotFound(format!("no preview {id}"))) };
            let mut vars: std::collections::BTreeMap<String, String> = raw.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
            f(&mut vars)?;
            let raw = (!vars.is_empty()).then(|| serde_json::to_string(&vars).unwrap_or_default());
            c.execute("UPDATE previews SET vars = ? WHERE id = ?", params![raw, id])?;
            Ok(vars)
        })
    }

    /// When the preview goes unless it's used or kept, once that's close ([`crate::engine`]'s
    /// warning; `None`: not close).
    pub fn goes_at(&self, id: &str) -> Result<Option<i64>> {
        Ok(self.db.read(|c| c.query_row("SELECT goes_at FROM previews WHERE id = ?", [id], |r| r.get(0)).optional())?.flatten())
    }

    pub fn set_goes_at(&self, id: &str, at: Option<i64>) -> Result<()> {
        self.db.write(|c| Ok::<_, Error>(c.execute("UPDATE previews SET goes_at = ? WHERE id = ?", params![at, id])?))?;
        Ok(())
    }

    pub fn set_kept_until(&self, id: &str, until: Option<i64>) -> Result<()> {
        let n = self.db.write(|c| Ok::<_, Error>(c.execute("UPDATE previews SET kept_until = ? WHERE id = ?", params![until, id])?))?;
        if n == 0 {
            return Err(Error::NotFound(format!("no preview {id}")));
        }
        Ok(())
    }

    /// Whether the preview's branches were told it's idle (and nothing used it since).
    pub fn idled(&self, id: &str) -> Result<bool> {
        Ok(self.db.read(|c| c.query_row("SELECT idled FROM previews WHERE id = ?", [id], |r| r.get::<_, i64>(0)).optional())?.unwrap_or(0) != 0)
    }

    pub fn set_idled(&self, id: &str, idled: bool) -> Result<()> {
        self.db.write(|c| {
            c.execute("UPDATE previews SET idled = ? WHERE id = ?", params![idled as i64, id])?;
            Ok::<_, Error>(())
        })
    }

    /// Whether the preview's data is fresh (branched or reset) and its seed hasn't run on it.
    pub fn seed_needed(&self, id: &str) -> Result<bool> {
        Ok(self.db.read(|c| c.query_row("SELECT seed_needed FROM previews WHERE id = ?", [id], |r| r.get::<_, i64>(0)).optional())?.unwrap_or(0) != 0)
    }

    /// Whether the seed has yet to run. Fresh data (a new branch, a reset) needs the seed, and
    /// the release and the schema's read again.
    pub fn set_seed_needed(&self, id: &str, needed: bool) -> Result<()> {
        self.db.write(|c| {
            match needed {
                true => c.execute("UPDATE previews SET seed_needed = 1, released = NULL, schema_head = NULL WHERE id = ?", [id])?,
                false => c.execute("UPDATE previews SET seed_needed = 0 WHERE id = ?", [id])?,
            };
            Ok::<_, Error>(())
        })
    }

    /// The head the project's `release` last ran at (on this data).
    pub fn released(&self, id: &str) -> Result<Option<String>> {
        Ok(self.db.read(|c| c.query_row("SELECT released FROM previews WHERE id = ?", [id], |r| r.get(0)).optional())?.flatten())
    }

    pub fn set_released(&self, id: &str, head: &str) -> Result<()> {
        self.db.write(|c| {
            c.execute("UPDATE previews SET released = ? WHERE id = ?", [head, id])?;
            Ok::<_, Error>(())
        })
    }

    /// The head the schema's changes were read at.
    pub fn schema_head(&self, id: &str) -> Result<Option<String>> {
        Ok(self.db.read(|c| c.query_row("SELECT schema_head FROM previews WHERE id = ?", [id], |r| r.get(0)).optional())?.flatten())
    }

    pub fn set_schema_changes(&self, id: &str, head: &str, changes: Option<&[String]>) -> Result<()> {
        let json = changes.map(|c| serde_json::to_string(c).expect("strings serialize"));
        self.db.write(|c| {
            c.execute("UPDATE previews SET schema_head = ?, schema_changes = ? WHERE id = ?", params![head, json, id])?;
            Ok::<_, Error>(())
        })
    }

    /// The head a screenshot was last tried at (taken or not).
    pub fn screenshot_head(&self, id: &str) -> Result<Option<String>> {
        Ok(self.db.read(|c| c.query_row("SELECT tried FROM screenshots WHERE preview_id = ?", [id], |r| r.get(0)).optional())?.flatten())
    }

    pub fn set_screenshot(&self, id: &str, head: &str, png: &[u8], speed: Option<&crate::PageSpeed>) -> Result<()> {
        use base64::Engine as _;
        let b64e = base64::engine::general_purpose::STANDARD;
        let (b64, now) = (b64e.encode(png), self.now_ms()?);
        let speed = speed.and_then(|s| serde_json::to_string(s).ok());
        // The one it replaces, at another head, is kept as "before", with how much changed.
        let before: Option<(String, String, Option<String>)> =
            self.db.read(|c| c.query_row("SELECT head, png, speed FROM screenshots WHERE preview_id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional())?;
        let before = before.filter(|(h, p, _)| h != head && !p.is_empty());
        let change_bp = before.as_ref().and_then(|(_, p, _)| b64e.decode(p).ok()).and_then(|old| crate::visual::changed(&old, png)).map(|f| (f * 10_000.0).round() as i64);
        self.db.write(|c| {
            match &before {
                Some((prev_head, prev_png, prev_speed)) => c.execute(
                    "INSERT INTO screenshots (preview_id, head, png, at, tried, prev_head, prev_png, change_bp, speed, prev_speed) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
                     ON CONFLICT (preview_id) DO UPDATE SET head = excluded.head, png = excluded.png, at = excluded.at, tried = excluded.tried, \
                     prev_head = excluded.prev_head, prev_png = excluded.prev_png, change_bp = excluded.change_bp, speed = excluded.speed, prev_speed = excluded.prev_speed",
                    params![id, head, b64, now, head, prev_head, prev_png, change_bp, speed, prev_speed],
                )?,
                // A first one, or the same head again: what came before stays.
                None => c.execute(
                    "INSERT INTO screenshots (preview_id, head, png, at, tried, speed) VALUES (?, ?, ?, ?, ?, ?) \
                     ON CONFLICT (preview_id) DO UPDATE SET head = excluded.head, png = excluded.png, at = excluded.at, tried = excluded.tried, speed = excluded.speed",
                    params![id, head, b64, now, head, speed],
                )?,
            };
            Ok::<_, Error>(())
        })
    }

    /// How fast the page loaded at the latest screenshot, and at the one before.
    pub fn page_speed(&self, id: &str) -> Result<Option<crate::PageSpeedView>> {
        let row: Option<(Option<String>, Option<String>)> = self.db.read(|c| c.query_row("SELECT speed, prev_speed FROM screenshots WHERE preview_id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?))).optional())?;
        let Some((now, before)) = row else { return Ok(None) };
        let parse = |s: Option<String>| s.and_then(|s| serde_json::from_str::<crate::PageSpeed>(&s).ok());
        Ok(parse(now).map(|now| crate::PageSpeedView { now, before: parse(before) }))
    }

    /// How much the page changed since the screenshot before (its head, in basis points).
    pub fn visual_change(&self, id: &str) -> Result<Option<(String, i64)>> {
        let row: Option<(Option<String>, Option<i64>)> = self.db.read(|c| c.query_row("SELECT prev_head, change_bp FROM screenshots WHERE preview_id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?))).optional())?;
        Ok(row.and_then(|(h, bp)| Some((h?, bp?))))
    }

    /// The screenshot before the latest (another head's), for comparing.
    pub fn previous_screenshot(&self, id: &str) -> Result<Option<Vec<u8>>> {
        use base64::Engine as _;
        let row: Option<Option<String>> = self.db.read(|c| c.query_row("SELECT prev_png FROM screenshots WHERE preview_id = ?", [id], |r| r.get(0)).optional())?;
        Ok(row.flatten().and_then(|b64| base64::engine::general_purpose::STANDARD.decode(b64).ok()))
    }

    /// A screenshot was tried at `head` and not taken: not tried again there, and the last one
    /// taken (another head's) stays.
    pub fn set_screenshot_tried(&self, id: &str, head: &str) -> Result<()> {
        self.db.write(|c| {
            c.execute(
                "INSERT INTO screenshots (preview_id, head, png, at, tried) VALUES (?, '', '', 0, ?) ON CONFLICT (preview_id) DO UPDATE SET tried = excluded.tried",
                params![id, head],
            )?;
            Ok::<_, Error>(())
        })
    }

    /// The preview's screenshot: (when it was taken, the PNG).
    pub fn screenshot(&self, id: &str) -> Result<Option<(i64, Vec<u8>)>> {
        use base64::Engine as _;
        let row: Option<(i64, String)> = self.db.read(|c| c.query_row("SELECT at, png FROM screenshots WHERE preview_id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?))).optional())?;
        Ok(row.and_then(|(at, b64)| base64::engine::general_purpose::STANDARD.decode(b64).ok().map(|png| (at, png))))
    }

    /// The key screenshot links are signed with: made once, the same on every node.
    fn signing_key(&self) -> Result<Vec<u8>> {
        if let Some(k) = self.signing.get() {
            return Ok(k.clone());
        }
        let mut raw = [0u8; 32];
        getrandom::fill(&mut raw).expect("system randomness");
        let fresh = hex::encode(raw);
        self.db.write(|c| {
            c.execute("INSERT INTO keys (name, value) VALUES ('screenshots', ?) ON CONFLICT (name) DO NOTHING", [fresh.as_str()])?;
            Ok::<_, Error>(())
        })?;
        let v: String = self.db.read(|c| c.query_row("SELECT value FROM keys WHERE name = 'screenshots'", (), |r| r.get(0)))?;
        let key = hex::decode(v).map_err(|e| Error::Internal(format!("the screenshot key: {e}")))?;
        Ok(self.signing.get_or_init(|| key).clone())
    }

    /// A link to the preview's screenshot anyone can open until it expires (30 days after it was
    /// taken): `(expiry in ms, signature)`. `None` when there's no screenshot.
    pub fn screenshot_link(&self, id: &str) -> Result<Option<(i64, String)>> {
        use hmac::Mac;
        let Some(at) = self.screenshot_at(id)? else { return Ok(None) };
        let exp = at + SCREENSHOT_LINK_MS;
        Ok(Some((exp, hex::encode(self.link_mac(id, exp)?.finalize().into_bytes()))))
    }

    /// Whether `sig` is the screenshot link's signature for `exp`, it hasn't expired, it's for the
    /// screenshot there is now (a link names one screenshot: `exp` is 30 days after it was taken),
    /// and the preview is still public and live.
    pub fn screenshot_link_ok(&self, id: &str, exp: i64, sig: &str) -> Result<bool> {
        use hmac::Mac;
        let Ok(sig) = hex::decode(sig) else { return Ok(false) };
        if exp <= self.now_ms()? || self.link_mac(id, exp)?.verify_slice(&sig).is_err() {
            return Ok(false);
        }
        if self.screenshot_at(id)? != Some(exp - SCREENSHOT_LINK_MS) {
            return Ok(false);
        }
        let Ok(row) = self.row(id) else { return Ok(false) };
        Ok(row.desired == Desired::Active && self.settings(&row.project_id)?.access == crate::settings::Access::Public)
    }

    fn link_mac(&self, id: &str, exp: i64) -> Result<hmac::Hmac<sha2::Sha256>> {
        use hmac::Mac;
        let mut m = hmac::Hmac::<sha2::Sha256>::new_from_slice(&self.signing_key()?).map_err(|e| Error::Internal(e.to_string()))?;
        m.update(format!("screenshot\n{id}\n{exp}").as_bytes());
        Ok(m)
    }

    fn screenshot_at(&self, id: &str) -> Result<Option<i64>> {
        Ok(self.db.read(|c| c.query_row("SELECT at FROM screenshots WHERE preview_id = ? AND png <> ''", [id], |r| r.get(0)).optional())?)
    }

    /// No schema changes shown (the head they were read at stays, so they're read again).
    pub fn clear_schema_changes(&self, id: &str) -> Result<()> {
        self.db.write(|c| {
            c.execute("UPDATE previews SET schema_changes = NULL WHERE id = ? AND schema_changes IS NOT NULL", [id])?;
            Ok::<_, Error>(())
        })
    }

    fn schema_changes(&self, id: &str) -> Result<Option<Vec<String>>> {
        let raw: Option<String> = self.db.read(|c| c.query_row("SELECT schema_changes FROM previews WHERE id = ?", [id], |r| r.get(0)).optional())?.flatten();
        Ok(raw.and_then(|j| serde_json::from_str(&j).ok()))
    }

    /// A preview's access token, and whether its project requires it.
    pub fn access(&self, id: &str) -> Result<Option<Access>> {
        let row: Option<(Option<String>, String, Option<String>, bool)> = self.db.read(|c| {
            c.query_row(
                "SELECT p.token, p.project_id, b.hash, p.trusted FROM previews p LEFT JOIN bypass b ON b.project_id = p.project_id WHERE p.id = ?",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
        })?;
        let Some((token, project, bypass, trusted)) = row else { return Ok(None) };
        let required = self.settings(&project)?.access == crate::settings::Access::Link;
        // A fork's preview never opens to the project's secret: CI that visits it would hand the
        // secret to the fork's code.
        Ok(Some(Access { token: token.unwrap_or_default(), required, bypass: bypass.filter(|_| trusted) }))
    }

    /// A new bypass secret for `project`'s private previews (replacing any): returned once,
    /// kept only as its SHA-256.
    pub fn rotate_bypass(&self, project: &str) -> Result<(String, i64)> {
        let secret = format!("rpb_{}", new_token());
        let (hash, now) = (bypass_hash(&secret), self.now_ms()?);
        self.db.write(|c| {
            c.execute(
                "INSERT INTO bypass (project_id, hash, created_at) VALUES (?, ?, ?) ON CONFLICT (project_id) DO UPDATE SET hash = excluded.hash, created_at = excluded.created_at",
                params![project, hash, now],
            )?;
            Ok::<_, Error>(())
        })?;
        Ok((secret, now))
    }

    pub fn delete_bypass(&self, project: &str) -> Result<()> {
        self.db.write(|c| {
            c.execute("DELETE FROM bypass WHERE project_id = ?", [project])?;
            Ok::<_, Error>(())
        })
    }

    /// When `project`'s bypass secret was made, if it has one.
    pub fn bypass_created(&self, project: &str) -> Result<Option<i64>> {
        Ok(self.db.read(|c| c.query_row("SELECT created_at FROM bypass WHERE project_id = ?", [project], |r| r.get(0)).optional())?)
    }

    /// Whether any preview but `except` has a service from `repo`.
    pub fn repo_has_other_services(&self, repo: &str, except: &str) -> Result<bool> {
        Ok(self.db.read(|c| c.query_row("SELECT 1 FROM services WHERE repo = ? AND preview_id <> ? LIMIT 1", [repo, except], |_| Ok(())).optional())?.is_some())
    }

    /// The preview and service a host label serves.
    /// The preview and service a host label is; an earlier push's host is `<service>@<revision>`.
    pub fn service_for_host(&self, host: &str) -> Result<Option<(String, String)>> {
        Ok(self.db.read(|c| {
            if let Some(found) = c.query_row("SELECT preview_id, name FROM services WHERE host = ?", [host], |r| Ok((r.get(0)?, r.get(1)?))).optional()? {
                return Ok(Some(found));
            }
            c.query_row("SELECT preview_id, service, revision FROM pinned WHERE host = ?", [host], |r| {
                Ok((r.get::<_, String>(0)?, format!("{}@{}", r.get::<_, String>(1)?, r.get::<_, String>(2)?)))
            })
            .optional()
        })?)
    }

    /// The host a service (or an earlier push's, `<service>@<revision>`) answers on.
    pub fn service_host(&self, id: &str, name: &str) -> Result<Option<String>> {
        Ok(self.db.read(|c| match name.split_once('@') {
            Some((svc, rev)) => c.query_row("SELECT host FROM pinned WHERE preview_id = ? AND service = ? AND revision = ?", [id, svc, rev], |r| r.get(0)).optional(),
            None => c.query_row("SELECT host FROM services WHERE preview_id = ? AND name = ?", [id, name], |r| r.get(0)).optional(),
        })?)
    }

    /// Keep `revision` of `service` reachable at a host of its own (an earlier push), and the
    /// [`PINNED_KEEP`] latest per service. Returns the ones that no longer fit, by
    /// `<service>@<revision>`, whose containers go.
    pub fn pin(&self, id: &str, service: &str, revision: &str, url: &dyn Fn(&str) -> String) -> Result<Vec<String>> {
        let now = self.now_ms()?;
        self.db.write(|c| {
            let slug: String = c.query_row("SELECT slug FROM previews WHERE id = ?", [id], |r| r.get(0))?;
            let short: String = revision.strip_prefix("cs_").unwrap_or(revision).chars().take(10).collect();
            let mut host = crate::label_part(&format!("{service}-{short}-{slug}"), 63);
            if host_taken(c, &host)? {
                host = format!("{}-{}", crate::label_part(&host, 58), &id[id.len() - 4..]);
            }
            c.execute(
                "INSERT INTO pinned (preview_id, service, revision, host, url, at) VALUES (?, ?, ?, ?, ?, ?) \
                 ON CONFLICT (preview_id, service, revision) DO UPDATE SET at = excluded.at",
                params![id, service, revision, host, url(&host), now],
            )?;
            let old: Vec<String> = c.query(
                "SELECT revision FROM pinned WHERE preview_id = ? AND service = ? ORDER BY at DESC LIMIT 1000 OFFSET ?",
                params![id, service, PINNED_KEEP as i64],
                |r| r.get(0),
            )?;
            for rev in &old {
                c.execute("DELETE FROM pinned WHERE preview_id = ? AND service = ? AND revision = ?", params![id, service, rev])?;
            }
            Ok::<_, Error>(old.into_iter().map(|rev| format!("{service}@{rev}")).collect())
        })
    }

    /// The earlier pushes a preview keeps reachable, newest first.
    pub fn pinned(&self, id: &str) -> Result<Vec<crate::Pinned>> {
        Ok(self.db.read(|c| {
            c.query("SELECT service, revision, url, at FROM pinned WHERE preview_id = ? ORDER BY at DESC", [id], |r| {
                Ok(crate::Pinned { service: r.get(0)?, revision: r.get(1)?, url: r.get(2)?, share_url: None, at: r.get(3)? })
            })
        })?)
    }

    // ── views ──

    /// The preview as the API shows it.
    pub fn preview(&self, id: &str) -> Result<Preview> {
        let r = self.row(id)?;
        self.view(r)
    }

    pub fn view(&self, r: Row) -> Result<Preview> {
        let resources = self.resources(&r.id)?;
        let services = self.services_with_time(&r.id)?;
        let mut cost = Cost::default();
        let now = self.now_ms()?;
        let month_start = month_start(now);
        let held_hours = (now - r.created_at.max(month_start)).max(0) as f64 / HOUR_MS;
        for res in &resources {
            cost.bytes += res.bytes;
            cost.sleeping_usd_month += res.usd_month;
            cost.active_usd_hour += res.usd_hour;
            // Storage since the start of the month at today's size, plus what the product
            // metered for compute.
            cost.month_to_date_usd += res.usd_month * held_hours / MONTH_HOURS + res.spent_usd;
        }
        for (s, active_ms) in &services {
            let gib = s.memory_mb as f64 / 1024.0;
            cost.active_usd_hour += gib * self.rates.usd_per_container_gib_hour;
            cost.month_to_date_usd += gib * self.rates.usd_per_container_gib_hour * (*active_ms as f64 / HOUR_MS);
        }
        // With link access, the links roll hands out carry the token (once; then a cookie).
        let token = match self.access(&r.id)? {
            Some(Access { token: t, required: true, .. }) if !t.is_empty() => Some(t),
            _ => None,
        };
        let share = |u: &str| match &token {
            Some(t) => format!("{}/?roll_access={t}", u.trim_end_matches('/')),
            None => u.to_string(),
        };
        let url = services.first().map(|(s, _)| s.url.clone());
        let share_url = url.as_deref().map(share);
        let schema_changes = self.schema_changes(&r.id)?;
        let kept_until = self.kept_until(&r.id)?;
        let goes_at = self.goes_at(&r.id)?;
        let screenshot_at = self.screenshot_at(&r.id)?;
        let visual_change = self.visual_change(&r.id)?.map(|(since, bp)| crate::VisualChange { since, percent: bp as f64 / 100.0 });
        let page_speed = self.page_speed(&r.id)?;
        let screenshot_link = self.screenshot_link(&r.id)?;
        let earlier = self.pinned(&r.id)?.into_iter().map(|mut p| {
            p.share_url = Some(share(&p.url));
            p
        });
        let earlier = earlier.collect();
        Ok(Preview {
            id: r.id,
            project: r.project_id,
            slug: r.slug,
            repo: r.repo,
            change: r.change_id,
            number: r.number,
            github_pr: r.github_pr,
            branch: r.branch,
            title: r.title,
            runs: r.runs,
            head: r.head,
            trusted: r.trusted,
            desired: r.desired,
            state: r.state,
            error: r.error,
            url,
            share_url,
            services: services
                .into_iter()
                .map(|(mut s, _)| {
                    s.share_url = Some(share(&s.url));
                    s
                })
                .collect(),
            resources: resources
                .into_iter()
                .map(|x| Resource { kind: x.kind, parent: x.parent, r#ref: x.r#ref, mode: x.mode, state: x.state, error: x.error, bytes: x.bytes })
                .collect(),
            cost: cost.rounded(),
            schema_changes,
            kept_until,
            goes_at,
            earlier,
            screenshot_at,
            visual_change,
            page_speed,
            screenshot_link,
            last_reset_at: r.last_reset_at,
            last_active_at: r.last_active_at,
            expires_at: r.expires_at,
            created_by: r.created_by,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
    }
}

/// A preview's access token: 24 random characters of lowercase base32 (120 bits).
fn new_token() -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut b = [0u8; 24];
    getrandom::fill(&mut b).expect("system randomness");
    b.iter().map(|x| ALPHABET[(x & 31) as usize] as char).collect()
}

fn slug_taken(c: &Conn<'_>, slug: &str) -> code_sql::Result<bool> {
    Ok(c.query_row("SELECT 1 FROM previews WHERE slug = ?", [slug], |_| Ok(())).optional()?.is_some())
}

fn host_taken(c: &Conn<'_>, host: &str) -> code_sql::Result<bool> {
    Ok(c.query_row("SELECT 1 FROM services WHERE host = ? UNION ALL SELECT 1 FROM pinned WHERE host = ? LIMIT 1", [host, host], |_| Ok(())).optional()?.is_some())
}

/// Earlier pushes kept reachable per service.
pub const PINNED_KEEP: usize = 3;

fn append(c: &Conn<'_>, id: &str, at: i64, message: &str) -> code_sql::Result<()> {
    let seq: i64 = c.query_row("SELECT COALESCE(MAX(seq), 0) + 1 FROM activity WHERE preview_id = ?", [id], |r| r.get(0))?;
    let message: String = message.chars().take(2000).collect();
    c.execute("INSERT INTO activity (preview_id, seq, at, message) VALUES (?, ?, ?, ?)", params![id, seq, at, message])?;
    c.execute("DELETE FROM activity WHERE preview_id = ? AND seq <= ?", params![id, seq - ACTIVITY_KEEP])?;
    Ok(())
}

/// The first millisecond of `now`'s month (UTC).
pub fn month_start(now_ms: i64) -> i64 {
    let days = now_ms.div_euclid(86_400_000);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    (days - (d - 1)) * 86_400_000
}

#[cfg(test)]
mod tests {
    /// The pull request comment's screenshot link opens that screenshot of a public preview
    /// until it expires; a link anyone changed (another preview, a later expiry, a guessed
    /// signature), an older screenshot's link, or a preview gone private doesn't.
    #[test]
    fn a_new_heads_screenshot_keeps_the_one_before_and_how_much_changed() {
        let png = |dark: u32| {
            let img = image::RgbaImage::from_fn(10, 10, |x, _| if x < dark { image::Rgba([0, 0, 0, 255]) } else { image::Rgba([255, 255, 255, 255]) });
            let mut out = std::io::Cursor::new(Vec::new());
            img.write_to(&mut out, image::ImageFormat::Png).unwrap();
            out.into_inner()
        };
        let (_dir, store) = store();
        let (row, _) = store.create(&new("chg_visual")).unwrap();
        let id = row.id.as_str();
        let speed = |load_ms| crate::PageSpeed { ttfb_ms: 20, fcp_ms: Some(100), load_ms, bytes: 1000 };
        store.set_screenshot(id, "cs_1", &png(0), Some(&speed(900))).unwrap();
        assert_eq!(store.visual_change(id).unwrap(), None, "a first one: nothing to compare");
        assert_eq!(store.page_speed(id).unwrap(), Some(crate::PageSpeedView { now: speed(900), before: None }));
        store.set_screenshot(id, "cs_2", &png(3), Some(&speed(1200))).unwrap();
        assert_eq!(store.visual_change(id).unwrap(), Some(("cs_1".to_string(), 3000)), "30% of pixels");
        assert_eq!(store.previous_screenshot(id).unwrap(), Some(png(0)));
        assert_eq!(store.page_speed(id).unwrap(), Some(crate::PageSpeedView { now: speed(1200), before: Some(speed(900)) }));
        // Taken again at the same head: what came before stays.
        store.set_screenshot(id, "cs_2", &png(3), Some(&speed(1100))).unwrap();
        assert_eq!(store.visual_change(id).unwrap(), Some(("cs_1".to_string(), 3000)));
        assert_eq!(store.page_speed(id).unwrap().unwrap().before, Some(speed(900)));
    }

    #[test]
    fn a_screenshot_link_is_signed_and_expires() {
        let (_dir, store) = store();
        store.put_settings("prj_1", &crate::Settings { access: crate::settings::Access::Public, ..Default::default() }).unwrap();
        let (row, _) = store.create(&new("chg_shot")).unwrap();
        let id = row.id.as_str();
        assert_eq!(store.screenshot_link(id).unwrap(), None, "no screenshot, no link");
        store.set_screenshot(id, "cs_1", b"\x89PNG...", None).unwrap();
        let (exp, sig) = store.screenshot_link(id).unwrap().unwrap();
        assert!(store.screenshot_link_ok(id, exp, &sig).unwrap());
        assert!(!store.screenshot_link_ok("pv_other", exp, &sig).unwrap(), "another preview");
        assert!(!store.screenshot_link_ok(id, exp + 1, &sig).unwrap(), "a moved expiry");
        assert!(!store.screenshot_link_ok(id, exp, &"0".repeat(64)).unwrap(), "a guess");
        let past = store.now_ms().unwrap() - 1;
        let stale = hex::encode({
            use hmac::Mac;
            store.link_mac(id, past).unwrap().finalize().into_bytes()
        });
        assert!(!store.screenshot_link_ok(id, past, &stale).unwrap(), "expired");
        // A failed try keeps the screenshot (and its link); a private preview's link is dead.
        store.set_screenshot_tried(id, "cs_2").unwrap();
        assert!(store.screenshot_link_ok(id, exp, &sig).unwrap(), "a failed try keeps the last one");
        store.put_settings("prj_1", &crate::Settings { access: crate::settings::Access::Link, ..Default::default() }).unwrap();
        assert!(!store.screenshot_link_ok(id, exp, &sig).unwrap(), "gone private");
    }

    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&Target::Sqlite(dir.path().join("p.db")), Default::default()).unwrap();
        (dir, s)
    }

    fn new(change: &str) -> New {
        New {
            project_id: "prj_1".into(),
            key: "pr7".into(),
            project_name: "shop".into(),
            repo: "acme/web".into(),
            change_id: Some(change.into()),
            branch: Some("feat".into()),
            number: Some(7),
            title: "t".into(),
            github_pr: Some(7),
            head: "cs_1".into(),
            trusted: true,
            state: State::Creating,
            expires_at: None,
            created_by: "roll".into(),
        }
    }

    #[test]
    fn one_preview_per_change_with_unique_slugs_and_hosts() {
        let (_d, s) = store();
        let (a, made) = s.create(&new("chg_1")).unwrap();
        assert!(made);
        assert_eq!(a.slug, "pr7-shop");
        let (again, made) = s.create(&new("chg_1")).unwrap();
        assert!(!made && again.id == a.id, "the change's preview, not a second one");
        let (b, _) = s.create(&new("chg_2")).unwrap();
        assert_ne!(b.slug, a.slug);
        assert!(b.slug.starts_with("pr7-shop-"));
        let url = |h: &str| format!("http://{h}.preview.test");
        let web = s.want_service(&a.id, "web", "acme/web", "cs_1", 512, &url).unwrap();
        assert_eq!(web.host, "web-pr7-shop");
        assert_eq!(web.url, "http://web-pr7-shop.preview.test");
        assert_eq!(s.service_for_host("web-pr7-shop").unwrap(), Some((a.id.clone(), "web".into())));
        // The branched resource names its preview, for scoped tokens.
        s.want_resources(&a.id, &[(Kind::Postgres, "prj_db".into(), Mode::Branch)]).unwrap();
        s.set_resource(&a.id, Kind::Postgres, "prj_db", |r| r.r#ref = Some("br_9".into())).unwrap();
        assert_eq!(s.environment_of("postgres", "br_9").unwrap(), Some(Environment { preview_id: a.id.clone(), project_id: "prj_1".into() }));
        assert_eq!(s.environment_of("postgres", "prj_db").unwrap(), None, "the parent isn't a preview's");
        s.remove(&a.id).unwrap();
        assert_eq!(s.environment_of("postgres", "br_9").unwrap(), None);
    }

    #[test]
    fn months_start_on_the_first() {
        // 2026-11-12T19:00:00Z → 2026-11-01T00:00:00Z
        assert_eq!(month_start(1_794_510_000_000), 1_793_491_200_000);
        assert_eq!(month_start(1_793_491_200_000), 1_793_491_200_000);
    }
}

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
CREATE TABLE IF NOT EXISTS shares (
    id TEXT PRIMARY KEY,
    preview_id TEXT NOT NULL,
    hash TEXT NOT NULL UNIQUE,
    expires_at INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    revoked INTEGER NOT NULL DEFAULT 0
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
CREATE TABLE IF NOT EXISTS page_shots (
    preview_id TEXT NOT NULL,
    path TEXT NOT NULL,
    head TEXT NOT NULL,
    png TEXT NOT NULL,
    prev_head TEXT,
    prev_png TEXT,
    change_bp INTEGER,
    errors INTEGER NOT NULL DEFAULT 0,
    base_bp INTEGER,
    PRIMARY KEY (preview_id, path)
);
CREATE TABLE IF NOT EXISTS base_history (
    preview_id TEXT NOT NULL,
    head TEXT NOT NULL,
    png TEXT NOT NULL,
    phone_png TEXT,
    pages TEXT NOT NULL,
    at INTEGER NOT NULL,
    PRIMARY KEY (preview_id, head)
);
CREATE TABLE IF NOT EXISTS visual_accepts (
    preview_id TEXT PRIMARY KEY,
    head TEXT NOT NULL,
    by TEXT NOT NULL,
    at INTEGER NOT NULL
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

    /// The default branch's preview roll keeps to compare changes with (`github.baseline`).
    pub fn is_baseline(&self) -> bool {
        self.change_id.is_none() && self.created_by == BASELINE_BY
    }
}

/// A preview's screenshot row without its PNGs ([`Store::shot_facts`]).
#[derive(Default)]
struct ShotFacts {
    /// When it was taken (none: no screenshot).
    at: Option<i64>,
    head: Option<String>,
    prev_head: Option<String>,
    change_bp: Option<i64>,
    phone_change_bp: Option<i64>,
    has_phone: bool,
    speed: Option<crate::PageSpeed>,
    prev_speed: Option<crate::PageSpeed>,
    health: Option<crate::PageHealth>,
    a11y: Option<crate::PageA11y>,
    base: Option<BaseRow>,
}

/// How many of a baseline's heads keep their screenshots (to compare changes at their merge base).
pub const BASE_HISTORY: usize = 10;

/// A preview's screenshots, base64 as stored: desktop, phone, and its other pages by path.
struct Shots {
    head: String,
    png: String,
    phone: Option<String>,
    pages: std::collections::BTreeMap<String, String>,
    /// How the page loaded and what it missed, as stored (`PageSpeed`, `PageA11y` JSON).
    speed: Option<String>,
    a11y: Option<String>,
    health: Option<String>,
}

/// Who made a baseline (`created_by`).
pub const BASELINE_BY: &str = "roll";

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
        for (col, ty) in [("tried", "TEXT"), ("prev_head", "TEXT"), ("prev_png", "TEXT"), ("change_bp", "INTEGER"), ("speed", "TEXT"), ("prev_speed", "TEXT"), ("health", "TEXT"), ("phone_png", "TEXT"), ("prev_phone_png", "TEXT"), ("phone_change_bp", "INTEGER"), ("a11y", "TEXT"), ("base_preview", "TEXT"), ("base_branch", "TEXT"), ("base_change_bp", "INTEGER"), ("base_phone_bp", "INTEGER"), ("base_head", "TEXT"), ("base_speed", "TEXT"), ("base_a11y", "TEXT"), ("base_health", "TEXT")] {
            if !db.has_column("screenshots", col)? {
                db.add_column("screenshots", col, ty)?;
            }
        }
        for col in ["speed", "a11y", "health"] {
            if !db.has_column("base_history", col)? {
                db.add_column("base_history", col, "TEXT")?;
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
            for t in ["resources", "services", "pinned", "shares", "activity", "screenshots", "page_shots", "visual_accepts", "base_history", "previews"] {
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

    pub fn set_screenshot(&self, id: &str, head: &str, png: &[u8], speed: Option<&crate::PageSpeed>, phone: Option<&[u8]>) -> Result<()> {
        use base64::Engine as _;
        let b64e = base64::engine::general_purpose::STANDARD;
        let (b64, now) = (b64e.encode(png), self.now_ms()?);
        let phone_b64 = phone.map(|p| b64e.encode(p));
        let speed = speed.and_then(|s| serde_json::to_string(s).ok());
        // The one it replaces, at another head, is kept as "before", with how much changed (at
        // both widths).
        type Before = (String, String, Option<String>, Option<String>);
        let before: Option<Before> = self
            .db
            .read(|c| c.query_row("SELECT head, png, speed, phone_png FROM screenshots WHERE preview_id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional())?;
        let before = before.filter(|(h, p, _, _)| h != head && !p.is_empty());
        let changed = |old: Option<&String>, new: Option<&[u8]>| -> Option<i64> {
            let old = b64e.decode(old?).ok()?;
            crate::visual::changed(&old, new?).map(|f| (f * 10_000.0).round() as i64)
        };
        let change_bp = before.as_ref().and_then(|(_, p, _, _)| changed(Some(p), Some(png)));
        let phone_change_bp = before.as_ref().and_then(|(_, _, _, pp)| changed(pp.as_ref(), phone));
        self.db.write(|c| {
            match &before {
                Some((prev_head, prev_png, prev_speed, prev_phone)) => c.execute(
                    "INSERT INTO screenshots (preview_id, head, png, at, tried, prev_head, prev_png, change_bp, speed, prev_speed, phone_png, prev_phone_png, phone_change_bp) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
                     ON CONFLICT (preview_id) DO UPDATE SET head = excluded.head, png = excluded.png, at = excluded.at, tried = excluded.tried, \
                     prev_head = excluded.prev_head, prev_png = excluded.prev_png, change_bp = excluded.change_bp, speed = excluded.speed, prev_speed = excluded.prev_speed, \
                     phone_png = excluded.phone_png, prev_phone_png = excluded.prev_phone_png, phone_change_bp = excluded.phone_change_bp",
                    params![id, head, b64, now, head, prev_head, prev_png, change_bp, speed, prev_speed, phone_b64, prev_phone, phone_change_bp],
                )?,
                // A first one, or the same head again: what came before stays.
                None => c.execute(
                    "INSERT INTO screenshots (preview_id, head, png, at, tried, speed, phone_png) VALUES (?, ?, ?, ?, ?, ?, ?) \
                     ON CONFLICT (preview_id) DO UPDATE SET head = excluded.head, png = excluded.png, at = excluded.at, tried = excluded.tried, speed = excluded.speed, \
                     phone_png = excluded.phone_png",
                    params![id, head, b64, now, head, speed, phone_b64],
                )?,
            };
            Ok::<_, Error>(())
        })
    }

    /// What went wrong while the latest screenshot's page loaded (after `set_screenshot`).
    pub fn set_page_report(&self, id: &str, health: Option<&crate::PageHealth>, a11y: Option<&crate::PageA11y>) -> Result<()> {
        let health = health.and_then(|h| serde_json::to_string(h).ok());
        let a11y = a11y.and_then(|a| serde_json::to_string(a).ok());
        self.db.write(|c| Ok::<_, Error>(c.execute("UPDATE screenshots SET health = ?, a11y = ? WHERE preview_id = ?", params![health, a11y, id])?))?;
        Ok(())
    }

    pub fn page_a11y(&self, id: &str) -> Result<Option<crate::PageA11y>> {
        let row: Option<Option<String>> = self.db.read(|c| c.query_row("SELECT a11y FROM screenshots WHERE preview_id = ?", [id], |r| r.get(0)).optional())?;
        Ok(row.flatten().and_then(|s| serde_json::from_str(&s).ok()))
    }

    pub fn page_health(&self, id: &str) -> Result<Option<crate::PageHealth>> {
        let row: Option<Option<String>> = self.db.read(|c| c.query_row("SELECT health FROM screenshots WHERE preview_id = ?", [id], |r| r.get(0)).optional())?;
        Ok(row.flatten().and_then(|s| serde_json::from_str(&s).ok()))
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

    /// Keep baseline `id`'s latest screenshots (desktop, phone, other pages) as its history at
    /// `head`, so changes that left the default branch there are compared with them; the last
    /// [`BASE_HISTORY`] heads stay.
    pub fn keep_base_history(&self, id: &str, head: &str) -> Result<()> {
        let Some(shots) = self.latest_shots(id)? else { return Ok(()) };
        let pages = serde_json::to_string(&shots.pages).map_err(|e| Error::Internal(e.to_string()))?;
        let now = self.now_ms()?;
        self.db.write(|c| {
            c.execute(
                "INSERT INTO base_history (preview_id, head, png, phone_png, pages, at, speed, a11y, health) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT (preview_id, head) DO UPDATE SET png = excluded.png, phone_png = excluded.phone_png, pages = excluded.pages, at = excluded.at, speed = excluded.speed, \
                 a11y = excluded.a11y, health = excluded.health",
                params![id, head, shots.png, shots.phone, pages, now, shots.speed, shots.a11y, shots.health],
            )?;
            let old: Vec<String> = c.query(&format!("SELECT head FROM base_history WHERE preview_id = ? ORDER BY at DESC LIMIT -1 OFFSET {BASE_HISTORY}"), [id], |r| r.get(0))?;
            for h in &old {
                c.execute("DELETE FROM base_history WHERE preview_id = ? AND head = ?", [id, h.as_str()])?;
            }
            Ok::<_, Error>(())
        })
    }

    /// A preview's latest screenshots, base64: desktop, phone, and its other pages by path.
    fn latest_shots(&self, id: &str) -> Result<Option<Shots>> {
        type Raw = (String, String, Option<String>, Option<String>, Option<String>, Option<String>);
        let row: Option<Raw> = self.db.read(|c| {
            c.query_row("SELECT head, png, phone_png, speed, a11y, health FROM screenshots WHERE preview_id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))).optional()
        })?;
        let Some((head, png, phone, speed, a11y, health)) = row else { return Ok(None) };
        let pages: Vec<(String, String)> = self.db.read(|c| c.query("SELECT path, png FROM page_shots WHERE preview_id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?))))?;
        Ok(Some(Shots { head, png, phone, pages: pages.into_iter().collect(), speed, a11y, health }))
    }

    /// Baseline `id`'s screenshots at `head`, from its history.
    fn shots_at(&self, id: &str, head: &str) -> Result<Option<Shots>> {
        type Raw = (String, Option<String>, String, Option<String>, Option<String>, Option<String>);
        let row: Option<Raw> = self.db.read(|c| {
            c.query_row("SELECT png, phone_png, pages, speed, a11y, health FROM base_history WHERE preview_id = ? AND head = ?", [id, head], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
            })
            .optional()
        })?;
        Ok(row.map(|(png, phone, pages, speed, a11y, health)| Shots { head: head.to_string(), png, phone, pages: serde_json::from_str(&pages).unwrap_or_default(), speed, a11y, health }))
    }

    /// Compare preview `id`'s latest screenshots (desktop, phone, other pages) with preview
    /// `base`'s, the project's preview of `branch` (the default branch: Chromatic's baseline), and
    /// keep how much differs. With `at` (where the change left the branch) and the baseline's
    /// screenshots there still kept, with those (Chromatic's ancestor build): main's own changes
    /// since aren't the change's. Nothing kept when either has none.
    pub fn compare_with_base(&self, id: &str, base: &str, branch: &str, at: Option<&str>) -> Result<()> {
        use base64::Engine as _;
        let b64e = base64::engine::general_purpose::STANDARD;
        let theirs = match at {
            Some(h) => match self.shots_at(base, h)? {
                Some(s) => Some(s),
                None => self.latest_shots(base)?,
            },
            None => self.latest_shots(base)?,
        };
        let (Some(ours), Some(theirs)) = (self.latest_shots(id)?, theirs) else { return Ok(()) };
        let bp = |a: Option<&String>, b: Option<&String>| -> Option<i64> {
            let (a, b) = (b64e.decode(a?).ok()?, b64e.decode(b?).ok()?);
            crate::visual::changed(&b, &a).map(|f| (f * 10_000.0).round() as i64)
        };
        let (change, phone) = (bp(Some(&ours.png), Some(&theirs.png)), bp(ours.phone.as_ref(), theirs.phone.as_ref()));
        // The other pages, each against the baseline's page at the same path.
        let page_bps: Vec<(String, Option<i64>)> = ours.pages.iter().map(|(path, png)| (path.clone(), bp(Some(png), theirs.pages.get(path)))).collect();
        self.db.write(|c| {
            c.execute(
                "UPDATE screenshots SET base_preview = ?, base_branch = ?, base_change_bp = ?, base_phone_bp = ?, base_head = ?, base_speed = ?, base_a11y = ?, base_health = ? WHERE preview_id = ?",
                params![base, branch, change, phone, theirs.head, theirs.speed, theirs.a11y, theirs.health, id],
            )?;
            for (path, bp) in &page_bps {
                c.execute("UPDATE page_shots SET base_bp = ? WHERE preview_id = ? AND path = ?", params![bp, id, path])?;
            }
            Ok::<_, Error>(())
        })
    }

    /// The baseline's screenshot a change was compared with (`page`: another page; `phone`: at a
    /// phone's width): from its history when it was compared at an older head.
    pub fn base_screenshot(&self, id: &str, phone: bool, page: Option<&str>) -> Result<Option<Vec<u8>>> {
        use base64::Engine as _;
        let Some(b) = self.base_change(id)? else { return Ok(None) };
        let shots = match &b.head {
            Some(h) => match self.shots_at(&b.preview, h)? {
                Some(s) => Some(s),
                None => self.latest_shots(&b.preview)?,
            },
            None => self.latest_shots(&b.preview)?,
        };
        let Some(s) = shots else { return Ok(None) };
        let b64 = match (page, phone) {
            (Some(p), _) => s.pages.get(p).cloned(),
            (None, true) => s.phone,
            (None, false) => Some(s.png),
        };
        Ok(b64.and_then(|v| base64::engine::general_purpose::STANDARD.decode(v).ok()))
    }

    /// How much the page differs from the default branch's preview (its branch, basis points
    /// at both widths), that preview, and the head of it compared with.
    pub fn base_change(&self, id: &str) -> Result<Option<BaseRow>> {
        Ok(self.shot_facts(id)?.base)
    }

    /// What a preview's screenshot row says, without its PNGs: one read for the view (a list of
    /// previews is one of these each, not twenty, and never the images).
    fn shot_facts(&self, id: &str) -> Result<ShotFacts> {
        fn json<T: serde::de::DeserializeOwned>(s: Option<String>) -> Option<T> {
            s.and_then(|s| serde_json::from_str(&s).ok())
        }
        let row = self.db.read(|c| {
            c.query_row(
                "SELECT CASE WHEN png <> '' THEN at END, head, prev_head, change_bp, phone_change_bp, phone_png IS NOT NULL, speed, prev_speed, health, a11y, \
                 base_preview, base_branch, base_change_bp, base_phone_bp, base_head, base_speed, base_a11y, base_health FROM screenshots WHERE preview_id = ?",
                [id],
                |r| {
                    let base = match (r.get::<_, Option<String>>(10)?, r.get::<_, Option<String>>(11)?, r.get::<_, Option<i64>>(12)?) {
                        (Some(preview), Some(branch), Some(change_bp)) => Some(BaseRow {
                            preview,
                            branch,
                            change_bp,
                            phone_bp: r.get(13)?,
                            head: r.get(14)?,
                            speed: json(r.get(15)?),
                            a11y: json(r.get(16)?),
                            health: json(r.get(17)?),
                        }),
                        _ => None,
                    };
                    Ok(ShotFacts {
                        at: r.get(0)?,
                        head: r.get(1)?,
                        prev_head: r.get(2)?,
                        change_bp: r.get(3)?,
                        phone_change_bp: r.get(4)?,
                        has_phone: r.get(5)?,
                        speed: json(r.get(6)?),
                        prev_speed: json(r.get(7)?),
                        health: json(r.get(8)?),
                        a11y: json(r.get(9)?),
                        base,
                    })
                },
            )
            .optional()
        })?;
        Ok(row.unwrap_or_default())
    }

    /// Keep the other pages a screenshot at `head` took (`github.pages`), each with the one it
    /// replaces at another head and how much changed; pages no longer in `wanted` go.
    pub fn set_page_shots(&self, id: &str, head: &str, pages: &[crate::ports::PageShot], wanted: &[String]) -> Result<()> {
        use base64::Engine as _;
        let b64e = base64::engine::general_purpose::STANDARD;
        let mut rows = Vec::new();
        for p in pages {
            let before: Option<(String, String)> = self.db.read(|c| c.query_row("SELECT head, png FROM page_shots WHERE preview_id = ? AND path = ?", [id, &p.path], |r| Ok((r.get(0)?, r.get(1)?))).optional())?;
            let before = before.filter(|(h, _)| h != head);
            let change_bp = before.as_ref().and_then(|(_, old)| crate::visual::changed(&b64e.decode(old).ok()?, &p.png)).map(|f| (f * 10_000.0).round() as i64);
            rows.push((p.path.clone(), b64e.encode(&p.png), p.errors as i64, before, change_bp));
        }
        self.db.write(|c| {
            for (path, png, errors, before, change_bp) in &rows {
                match before {
                    Some((prev_head, prev_png)) => c.execute(
                        "INSERT INTO page_shots (preview_id, path, head, png, prev_head, prev_png, change_bp, errors) VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
                         ON CONFLICT (preview_id, path) DO UPDATE SET head = excluded.head, png = excluded.png, prev_head = excluded.prev_head, prev_png = excluded.prev_png, \
                         change_bp = excluded.change_bp, errors = excluded.errors, base_bp = NULL",
                        params![id, path, head, png, prev_head, prev_png, change_bp, errors],
                    )?,
                    None => c.execute(
                        "INSERT INTO page_shots (preview_id, path, head, png, errors) VALUES (?, ?, ?, ?, ?) \
                         ON CONFLICT (preview_id, path) DO UPDATE SET head = excluded.head, png = excluded.png, errors = excluded.errors, base_bp = NULL",
                        params![id, path, head, png, errors],
                    )?,
                };
            }
            let paths: Vec<String> = c.query("SELECT path FROM page_shots WHERE preview_id = ?", [id], |r| r.get(0))?;
            for gone in paths.iter().filter(|p| !wanted.contains(p)) {
                c.execute("DELETE FROM page_shots WHERE preview_id = ? AND path = ?", [id, gone.as_str()])?;
            }
            Ok::<_, Error>(())
        })
    }

    /// The other pages' screenshots, by path: how each changed and compares with the baseline's.
    pub fn page_checks(&self, id: &str) -> Result<Vec<crate::PageCheck>> {
        type Raw = (String, i64, Option<String>, Option<i64>, Option<i64>);
        let rows: Vec<Raw> = self.db.read(|c| {
            c.query("SELECT path, errors, prev_head, change_bp, base_bp FROM page_shots WHERE preview_id = ? ORDER BY path", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
        })?;
        Ok(rows
            .into_iter()
            .map(|(path, errors, since, change, base)| crate::PageCheck {
                path,
                errors: errors as u32,
                since: since.filter(|_| change.is_some()),
                percent: change.map(|bp| bp as f64 / 100.0),
                base_percent: base.map(|bp| bp as f64 / 100.0),
            })
            .collect())
    }

    /// `by` accepted how the preview's pages look at the latest screenshot's head (`None`: no
    /// screenshot to accept).
    /// `seen`: the head whose screenshots were looked at; refused (`Conflict`) when a newer one
    /// came in meanwhile, so nobody accepts pages they didn't see.
    pub fn accept_visual(&self, id: &str, by: &str, seen: Option<&str>) -> Result<Option<String>> {
        let now = self.now_ms()?;
        // Read and written in one transaction: a screenshot landing in between can't be accepted.
        self.db.write(|c| {
            let head: Option<String> = c.query_row("SELECT head FROM screenshots WHERE preview_id = ?", [id], |r| r.get(0)).optional()?;
            let Some(head) = head else { return Ok(None) };
            if seen.is_some_and(|s| s != head) {
                return Err(Error::Conflict(format!("a newer screenshot came in (at {}): look at it first", head.chars().take(12).collect::<String>())));
            }
            c.execute(
                "INSERT INTO visual_accepts (preview_id, head, by, at) VALUES (?, ?, ?, ?) ON CONFLICT (preview_id) DO UPDATE SET head = excluded.head, by = excluded.by, at = excluded.at",
                params![id, head, by, now],
            )?;
            Ok::<_, Error>(Some(head))
        })
    }

    /// After a screenshot: an acceptance of the head just before moves to this one when every page
    /// looks exactly as it did there (Chromatic carries accepted baselines forward), so a push that
    /// changes nothing visible isn't reviewed again. `true` when it moved.
    pub fn carry_acceptance(&self, id: &str) -> Result<bool> {
        self.db.write(|c| {
            let accepted: Option<String> = c.query_row("SELECT head FROM visual_accepts WHERE preview_id = ?", [id], |r| r.get(0)).optional()?;
            // (head, the head before, desktop change, phone change)
            type Shot = (String, Option<String>, Option<i64>, Option<i64>);
            let shot: Option<Shot> =
                c.query_row("SELECT head, prev_head, change_bp, phone_change_bp FROM screenshots WHERE preview_id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()?;
            let (Some(accepted), Some((head, Some(prev), Some(0), phone))) = (accepted, shot) else { return Ok(false) };
            if accepted != prev || head == accepted || phone.is_some_and(|p| p != 0) {
                return Ok(false);
            }
            // Every other page was taken at both heads and is unchanged too.
            let pages: Vec<(Option<String>, Option<i64>)> = c.query("SELECT prev_head, change_bp FROM page_shots WHERE preview_id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?)))?;
            if pages.iter().any(|(h, bp)| h.as_deref() != Some(prev.as_str()) || *bp != Some(0)) {
                return Ok(false);
            }
            c.execute("UPDATE visual_accepts SET head = ? WHERE preview_id = ?", [head.as_str(), id])?;
            Ok::<_, Error>(true)
        })
    }

    /// The visual review of a change's preview (`github.visual_review`), once its pages were
    /// compared with the baseline's: the most any of them differs, and who accepted that head.
    fn visual_review(&self, r: &Row, base: Option<&crate::BaseChange>, pages: &[crate::PageCheck], shot: Option<String>) -> Result<Option<crate::VisualReview>> {
        let rule = self.settings(&r.project_id)?.visual_review;
        if r.change_id.is_none() || rule == crate::settings::VisualReviewRule::Off {
            return Ok(None);
        }
        let Some(base) = base else { return Ok(None) };
        let percent = [Some(base.percent), base.phone_percent].into_iter().chain(pages.iter().map(|p| p.base_percent)).flatten().fold(0.0_f64, f64::max);
        let accepted: Option<(String, String, i64)> = self.db.read(|c| c.query_row("SELECT head, by, at FROM visual_accepts WHERE preview_id = ?", [&r.id], |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?))).optional())?;
        let accepted = accepted.filter(|(h, _, _)| shot.as_deref() == Some(h.as_str()));
        let state = match (&accepted, percent > 0.0) {
            (_, false) => crate::VisualState::Unchanged,
            (Some(_), true) => crate::VisualState::Accepted,
            (None, true) => crate::VisualState::Changed,
        };
        let (by, at) = match accepted.filter(|_| state == crate::VisualState::Accepted) {
            Some((_, by, at)) => (Some(by), Some(at)),
            None => (None, None),
        };
        Ok(Some(crate::VisualReview { state, branch: base.branch.clone(), percent, by, at, required: rule == crate::settings::VisualReviewRule::Required, head: shot.unwrap_or_default() }))
    }

    /// Another page's screenshot (`previous`: the one before it).
    pub fn page_screenshot(&self, id: &str, path: &str, previous: bool) -> Result<Option<Vec<u8>>> {
        use base64::Engine as _;
        let col = if previous { "prev_png" } else { "png" };
        let row: Option<Option<String>> = self.db.read(|c| c.query_row(&format!("SELECT {col} FROM page_shots WHERE preview_id = ? AND path = ?"), [id, path], |r| r.get(0)).optional())?;
        Ok(row.flatten().and_then(|b64| base64::engine::general_purpose::STANDARD.decode(b64).ok()))
    }

    /// The phone-width screenshot (`previous`: the one before it), if one was taken.
    pub fn phone_screenshot(&self, id: &str, previous: bool) -> Result<Option<Vec<u8>>> {
        use base64::Engine as _;
        let col = if previous { "prev_phone_png" } else { "phone_png" };
        let row: Option<Option<String>> = self.db.read(|c| c.query_row(&format!("SELECT {col} FROM screenshots WHERE preview_id = ?"), [id], |r| r.get(0)).optional())?;
        Ok(row.flatten().and_then(|b64| base64::engine::general_purpose::STANDARD.decode(b64).ok()))
    }

    /// How much the phone-width page changed since the one before (basis points).
    pub fn phone_change(&self, id: &str) -> Result<Option<i64>> {
        let row: Option<Option<i64>> = self.db.read(|c| c.query_row("SELECT phone_change_bp FROM screenshots WHERE preview_id = ?", [id], |r| r.get(0)).optional())?;
        Ok(row.flatten())
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
        self.link(id, None)
    }

    /// The same for where its page differs from the baseline's (`diff=base`): its own signature,
    /// so a screenshot's link can't be turned into the diff's or back.
    pub fn diff_link(&self, id: &str) -> Result<Option<(i64, String)>> {
        self.link(id, Some("diff=base"))
    }

    fn link(&self, id: &str, variant: Option<&str>) -> Result<Option<(i64, String)>> {
        match self.screenshot_at(id)? {
            Some(at) => self.link_at(id, at, variant).map(Some),
            None => Ok(None),
        }
    }

    /// The link to the screenshot taken at `at`.
    fn link_at(&self, id: &str, at: i64, variant: Option<&str>) -> Result<(i64, String)> {
        use hmac::Mac;
        let exp = at + SCREENSHOT_LINK_MS;
        Ok((exp, hex::encode(self.link_mac(id, exp, variant)?.finalize().into_bytes())))
    }

    /// Whether `sig` is the screenshot link's signature for `exp`, it hasn't expired, it's for the
    /// screenshot there is now (a link names one screenshot: `exp` is 30 days after it was taken),
    /// and the preview is still public and live.
    pub fn screenshot_link_ok(&self, id: &str, exp: i64, sig: &str, variant: Option<&str>) -> Result<bool> {
        use hmac::Mac;
        let Ok(sig) = hex::decode(sig) else { return Ok(false) };
        if exp <= self.now_ms()? || self.link_mac(id, exp, variant)?.verify_slice(&sig).is_err() {
            return Ok(false);
        }
        if self.screenshot_at(id)? != Some(exp - SCREENSHOT_LINK_MS) {
            return Ok(false);
        }
        let Ok(row) = self.row(id) else { return Ok(false) };
        Ok(row.desired == Desired::Active && self.settings(&row.project_id)?.access == crate::settings::Access::Public)
    }

    fn link_mac(&self, id: &str, exp: i64, variant: Option<&str>) -> Result<hmac::Hmac<sha2::Sha256>> {
        use hmac::Mac;
        let mut m = hmac::Hmac::<sha2::Sha256>::new_from_slice(&self.signing_key()?).map_err(|e| Error::Internal(e.to_string()))?;
        // A plain screenshot's message is what it always was, so links already posted still work.
        match variant {
            None => m.update(format!("screenshot\n{id}\n{exp}").as_bytes()),
            Some(v) => m.update(format!("screenshot\n{id}\n{exp}\n{v}").as_bytes()),
        }
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

    /// A link that opens the preview until `expires_at` (Vercel's shareable links), on a token of
    /// its own: only its SHA-256 is kept, so the token is in the answer once. At most
    /// [`SHARES_MAX`] live ones.
    pub fn create_share(&self, id: &str, expires_at: i64, by: &str) -> Result<(crate::Share, String)> {
        let now = self.now_ms()?;
        let token = format!("rps_{}", new_token());
        let sid = format!("shr_{}", &new_token()[..12]);
        let hash = bypass_hash(&token);
        self.db.write(|c| {
            let live: i64 = c.query_row("SELECT COUNT(*) FROM shares WHERE preview_id = ? AND revoked = 0 AND expires_at > ?", params![id, now], |r| r.get(0))?;
            if live as usize >= SHARES_MAX {
                return Err(Error::Conflict(format!("the preview has {SHARES_MAX} live share links: revoke one first")));
            }
            c.execute("INSERT INTO shares (id, preview_id, hash, expires_at, created_by, created_at) VALUES (?, ?, ?, ?, ?, ?)", params![sid, id, hash, expires_at, by, now])?;
            Ok::<_, Error>(())
        })?;
        Ok((crate::Share { id: sid, expires_at, created_by: by.to_string(), created_at: now, revoked: false }, token))
    }

    /// The preview's share links, newest first (never their tokens).
    pub fn shares(&self, id: &str) -> Result<Vec<crate::Share>> {
        Ok(self.db.read(|c| {
            c.query("SELECT id, expires_at, created_by, created_at, revoked FROM shares WHERE preview_id = ? ORDER BY created_at DESC", [id], |r| {
                Ok(crate::Share { id: r.get(0)?, expires_at: r.get(1)?, created_by: r.get(2)?, created_at: r.get(3)?, revoked: r.get::<_, i64>(4)? != 0 })
            })
        })?)
    }

    pub fn revoke_share(&self, id: &str, share: &str) -> Result<()> {
        let n = self.db.write(|c| Ok::<_, Error>(c.execute("UPDATE shares SET revoked = 1 WHERE preview_id = ? AND id = ?", params![id, share])?))?;
        if n == 0 {
            return Err(Error::NotFound(format!("no share link {share}")));
        }
        Ok(())
    }

    /// When the share link `token` of preview `id` ends, if it's live (not revoked, not past).
    pub fn share_ends(&self, id: &str, token: &str) -> Result<Option<i64>> {
        let now = self.now_ms()?;
        Ok(self.db.read(|c| {
            c.query_row("SELECT expires_at FROM shares WHERE preview_id = ? AND hash = ? AND revoked = 0 AND expires_at > ?", params![id, bypass_hash(token), now], |r| r.get(0)).optional()
        })?)
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
        // The screenshot row in one read, without its images.
        let f = self.shot_facts(&r.id)?;
        let screenshot_at = f.at;
        let phone_percent = f.phone_change_bp.map(|bp| bp as f64 / 100.0);
        let visual_change = f.prev_head.clone().zip(f.change_bp).map(|(since, bp)| crate::VisualChange { since, percent: bp as f64 / 100.0, phone_percent });
        let phone_screenshot = f.has_phone;
        let pages = self.page_checks(&r.id)?;
        let baseline = r.is_baseline();
        let (page_a11y, page_health) = (f.a11y, f.health);
        let base_row = f.base;
        let base_differs = base_row.as_ref().is_some_and(|b| b.change_bp > 0);
        let base_change = base_row.map(|b| crate::BaseChange {
            branch: b.branch,
            percent: b.change_bp as f64 / 100.0,
            phone_percent: b.phone_bp.map(|p| p as f64 / 100.0),
            head: b.head,
            load_ms: b.speed.map(|s| s.load_ms),
            a11y_added: page_a11y.as_ref().zip(b.a11y.as_ref()).map(|(ours, main)| ours.added_since(main)),
            errors_added: page_health.as_ref().zip(b.health.as_ref()).map(|(ours, main)| ours.added_since(main)),
        });
        let visual_review = self.visual_review(&r, base_change.as_ref(), &pages, f.head)?;
        let page_speed = f.speed.map(|now| crate::PageSpeedView { now, before: f.prev_speed });
        let screenshot_link = screenshot_at.map(|at| self.link_at(&r.id, at, None)).transpose()?;
        let diff_link = match screenshot_at.filter(|_| base_differs) {
            Some(at) => Some(self.link_at(&r.id, at, Some("diff=base"))?),
            None => None,
        };
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
            phone_screenshot,
            base_change,
            pages,
            visual_review,
            page_speed,
            page_health,
            page_a11y,
            screenshot_link,
            diff_link,
            last_reset_at: r.last_reset_at,
            last_active_at: r.last_active_at,
            expires_at: r.expires_at,
            baseline,
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

/// How much a preview's page differs from its project's preview of the default branch.
#[derive(Clone, Debug, PartialEq)]
pub struct BaseRow {
    /// The default branch's preview.
    pub preview: String,
    pub branch: String,
    /// The head of it compared with (`None`: compared before roll kept it).
    pub head: Option<String>,
    /// How its first page loaded there, and what it missed (to say what the change added).
    pub speed: Option<crate::PageSpeed>,
    pub a11y: Option<crate::PageA11y>,
    pub health: Option<crate::PageHealth>,
    pub change_bp: i64,
    pub phone_bp: Option<i64>,
}

/// Live share links a preview may have at once.
pub const SHARES_MAX: usize = 20;

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
        store.set_screenshot(id, "cs_1", &png(0), Some(&speed(900)), Some(&png(0))).unwrap();
        assert_eq!(store.visual_change(id).unwrap(), None, "a first one: nothing to compare");
        assert_eq!(store.page_speed(id).unwrap(), Some(crate::PageSpeedView { now: speed(900), before: None }));
        store.set_screenshot(id, "cs_2", &png(3), Some(&speed(1200)), Some(&png(5))).unwrap();
        assert_eq!(store.visual_change(id).unwrap(), Some(("cs_1".to_string(), 3000)), "30% of pixels");
        assert_eq!(store.previous_screenshot(id).unwrap(), Some(png(0)));
        assert_eq!(store.page_speed(id).unwrap(), Some(crate::PageSpeedView { now: speed(1200), before: Some(speed(900)) }));
        // The phone-width one too, with its own change.
        assert_eq!(store.phone_change(id).unwrap(), Some(5000));
        assert_eq!(store.phone_screenshot(id, true).unwrap(), Some(png(0)));
        // Against the default branch's preview: its own share, at both widths.
        let (main, _) = store.create(&New { change_id: None, branch: Some("main".into()), key: "main".into(), ..new("chg_main") }).unwrap();
        assert!(store.compare_with_base(id, &main.id, "main", None).is_ok() && store.base_change(id).unwrap().is_none(), "no screenshot there yet");
        store.set_screenshot(&main.id, "cs_m", &png(1), None, Some(&png(0))).unwrap();
        store.keep_base_history(&main.id, "cs_m").unwrap();
        store.compare_with_base(id, &main.id, "main", None).unwrap();
        assert_eq!(store.base_change(id).unwrap(), Some(BaseRow { preview: main.id.clone(), branch: "main".into(), change_bp: 2000, phone_bp: Some(5000), head: Some("cs_m".into()), speed: None, a11y: None, health: None }));
        // Main moves on (to what the change's page looks like): a change that left it at cs_m is
        // still compared with cs_m's, and shown that one.
        store.set_screenshot(&main.id, "cs_m2", &png(3), Some(&speed(700)), Some(&png(5))).unwrap();
        store.keep_base_history(&main.id, "cs_m2").unwrap();
        store.compare_with_base(id, &main.id, "main", Some("cs_m")).unwrap();
        assert_eq!(store.base_change(id).unwrap().map(|b| (b.change_bp, b.head)), Some((2000, Some("cs_m".into()))));
        assert_eq!(store.base_screenshot(id, false, None).unwrap(), Some(png(1)));
        store.compare_with_base(id, &main.id, "main", Some("cs_gone")).unwrap();
        assert_eq!(store.base_change(id).unwrap().map(|b| (b.change_bp, b.head)), Some((0, Some("cs_m2".into()))), "not kept: the latest");
        // Its load there comes along (to say "slower than main"), from the history too.
        store.compare_with_base(id, &main.id, "main", Some("cs_m2")).unwrap();
        assert_eq!(store.base_change(id).unwrap().and_then(|b| b.speed).map(|s| s.load_ms), Some(700));
        // Taken again at the same head: what came before stays.
        store.set_screenshot(id, "cs_2", &png(3), Some(&speed(1100)), Some(&png(5))).unwrap();
        assert_eq!(store.visual_change(id).unwrap(), Some(("cs_1".to_string(), 3000)));
        assert_eq!(store.page_speed(id).unwrap().unwrap().before, Some(speed(900)));
    }

    #[test]
    fn a_share_link_opens_its_preview_until_it_ends_or_is_revoked() {
        let (_dir, store) = store();
        let (row, _) = store.create(&new("chg_share")).unwrap();
        let id = row.id.as_str();
        let now = store.now_ms().unwrap();
        let (share, token) = store.create_share(id, now + 3_600_000, "ada").unwrap();
        assert!(token.starts_with("rps_") && share.id.starts_with("shr_"));
        assert_eq!(store.share_ends(id, &token).unwrap(), Some(now + 3_600_000));
        assert_eq!(store.share_ends("pv_other", &token).unwrap(), None, "another preview");
        assert_eq!(store.share_ends(id, "rps_guess").unwrap(), None);
        let (_, ended) = store.create_share(id, now - 1, "ada").unwrap();
        assert_eq!(store.share_ends(id, &ended).unwrap(), None, "past its end");
        store.revoke_share(id, &share.id).unwrap();
        assert_eq!(store.share_ends(id, &token).unwrap(), None, "revoked");
        // Only its hash is kept: the list never has a token.
        let listed = serde_json::to_string(&store.shares(id).unwrap()).unwrap();
        assert!(!listed.contains(&token) && listed.contains("\"revoked\":true"));
    }

    #[test]
    fn a_screenshot_link_is_signed_and_expires() {
        let (_dir, store) = store();
        store.put_settings("prj_1", &crate::Settings { access: crate::settings::Access::Public, ..Default::default() }).unwrap();
        let (row, _) = store.create(&new("chg_shot")).unwrap();
        let id = row.id.as_str();
        assert_eq!(store.screenshot_link(id).unwrap(), None, "no screenshot, no link");
        store.set_screenshot(id, "cs_1", b"\x89PNG...", None, None).unwrap();
        let (exp, sig) = store.screenshot_link(id).unwrap().unwrap();
        assert!(store.screenshot_link_ok(id, exp, &sig, None).unwrap());
        assert!(!store.screenshot_link_ok("pv_other", exp, &sig, None).unwrap(), "another preview");
        assert!(!store.screenshot_link_ok(id, exp + 1, &sig, None).unwrap(), "a moved expiry");
        assert!(!store.screenshot_link_ok(id, exp, &"0".repeat(64), None).unwrap(), "a guess");
        // The diff's link is its own: neither opens the other.
        let (_, diff) = store.diff_link(id).unwrap().unwrap();
        assert!(store.screenshot_link_ok(id, exp, &diff, Some("diff=base")).unwrap());
        assert!(!store.screenshot_link_ok(id, exp, &diff, None).unwrap() && !store.screenshot_link_ok(id, exp, &sig, Some("diff=base")).unwrap());
        let past = store.now_ms().unwrap() - 1;
        let stale = hex::encode({
            use hmac::Mac;
            store.link_mac(id, past, None).unwrap().finalize().into_bytes()
        });
        assert!(!store.screenshot_link_ok(id, past, &stale, None).unwrap(), "expired");
        // A failed try keeps the screenshot (and its link); a private preview's link is dead.
        store.set_screenshot_tried(id, "cs_2").unwrap();
        assert!(store.screenshot_link_ok(id, exp, &sig, None).unwrap(), "a failed try keeps the last one");
        store.put_settings("prj_1", &crate::Settings { access: crate::settings::Access::Link, ..Default::default() }).unwrap();
        assert!(!store.screenshot_link_ok(id, exp, &sig, None).unwrap(), "gone private");
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

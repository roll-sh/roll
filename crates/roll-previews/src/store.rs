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
    revoked INTEGER NOT NULL DEFAULT 0,
    opened_day INTEGER
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS visit_tickets (
    hash TEXT PRIMARY KEY,
    preview_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    roll_session TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    handle TEXT NOT NULL DEFAULT ''
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS visit_sessions (
    hash TEXT PRIMARY KEY,
    preview_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    roll_session TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    checked_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS visit_sessions_project ON visit_sessions (project_id);
CREATE TABLE IF NOT EXISTS visits (
    preview_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    handle TEXT NOT NULL,
    first_at INTEGER NOT NULL,
    last_at INTEGER NOT NULL,
    PRIMARY KEY (preview_id, user_id)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS gone_pushes (
    host TEXT PRIMARY KEY,
    preview_id TEXT NOT NULL,
    service TEXT NOT NULL,
    revision TEXT NOT NULL,
    at INTEGER NOT NULL
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
CREATE TABLE IF NOT EXISTS events (
    seq INTEGER PRIMARY KEY,
    at INTEGER NOT NULL,
    kind TEXT NOT NULL,
    project_id TEXT NOT NULL,
    preview_id TEXT NOT NULL,
    change_id TEXT,
    data TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS events_at ON events (at);
CREATE TABLE IF NOT EXISTS pinned_data (
    preview_id TEXT NOT NULL,
    revision TEXT NOT NULL,
    kind TEXT NOT NULL,
    parent TEXT NOT NULL,
    name TEXT NOT NULL,
    ref TEXT NOT NULL,
    kept TEXT,
    at INTEGER NOT NULL,
    PRIMARY KEY (preview_id, revision, kind, parent)
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
CREATE TABLE IF NOT EXISTS visual_page_accepts (
    preview_id TEXT NOT NULL,
    path TEXT NOT NULL,
    head TEXT NOT NULL,
    by TEXT NOT NULL,
    at INTEGER NOT NULL,
    PRIMARY KEY (preview_id, path)
);
CREATE TABLE IF NOT EXISTS resolved_notes (
    preview_id TEXT NOT NULL,
    review_id TEXT NOT NULL,
    by TEXT NOT NULL,
    at INTEGER NOT NULL,
    PRIMARY KEY (preview_id, review_id)
);
CREATE TABLE IF NOT EXISTS mail (
    id TEXT PRIMARY KEY,
    preview_id TEXT NOT NULL,
    at INTEGER NOT NULL,
    sender TEXT NOT NULL,
    recipients TEXT NOT NULL,
    subject TEXT,
    size INTEGER NOT NULL,
    raw TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS mail_by_preview ON mail (preview_id, at);
CREATE TABLE IF NOT EXISTS budget_alerts (
    project_id TEXT NOT NULL,
    month INTEGER NOT NULL,
    max_usd REAL NOT NULL,
    level INTEGER NOT NULL,
    at INTEGER NOT NULL,
    PRIMARY KEY (project_id, month, max_usd, level)
);
CREATE TABLE IF NOT EXISTS hooks (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    name TEXT NOT NULL,
    branches TEXT NOT NULL,
    hash TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL,
    fired_at INTEGER
);

CREATE TABLE IF NOT EXISTS project_domains (
    project_id TEXT PRIMARY KEY,
    base TEXT NOT NULL UNIQUE,
    https INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS git_skips (
    project_id TEXT NOT NULL,
    repo TEXT NOT NULL,
    branch TEXT NOT NULL,
    PRIMARY KEY (project_id, repo, branch)
);
CREATE TABLE IF NOT EXISTS bypass (
    project_id TEXT PRIMARY KEY,
    hash TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS runtime_facts (
    preview_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    json TEXT NOT NULL,
    PRIMARY KEY (preview_id, kind)
);
CREATE TABLE IF NOT EXISTS rules_makers (
    project_id TEXT PRIMARY KEY,
    by TEXT NOT NULL,
    at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS env_refs (
    project_id TEXT NOT NULL,
    scope TEXT NOT NULL,
    name TEXT NOT NULL,
    reference TEXT NOT NULL,
    by TEXT NOT NULL,
    at INTEGER NOT NULL,
    PRIMARY KEY (project_id, scope, name)
);
CREATE TABLE IF NOT EXISTS git_reports (
    preview_id TEXT PRIMARY KEY,
    status TEXT,
    comment_id INTEGER,
    comment_hash TEXT
);
CREATE TABLE IF NOT EXISTS pull_comments (
    service TEXT NOT NULL,
    repo TEXT NOT NULL,
    number INTEGER NOT NULL,
    comment_id INTEGER NOT NULL,
    PRIMARY KEY (service, repo, number)
);
CREATE TABLE IF NOT EXISTS git_pulls (
    preview_id TEXT PRIMARY KEY,
    number INTEGER NOT NULL,
    url TEXT NOT NULL,
    fork INTEGER NOT NULL,
    approved_head TEXT,
    approved_by TEXT
);
CREATE TABLE IF NOT EXISTS note_crops (
    preview_id TEXT NOT NULL,
    note_id TEXT NOT NULL,
    png TEXT NOT NULL,
    at INTEGER NOT NULL,
    PRIMARY KEY (preview_id, note_id)
);
CREATE TABLE IF NOT EXISTS leases (
    preview_id TEXT NOT NULL,
    service TEXT NOT NULL,
    lease TEXT NOT NULL,
    issued_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    PRIMARY KEY (preview_id, lease)
);
";

/// [`Store::set_page_shots`]'s rows, prepared: path, PNG, errors, the one before, change, report.
type PageShotRow = (String, String, i64, Option<(String, String)>, Option<i64>, Option<String>);

fn write_page_shots(c: &Conn<'_>, id: &str, head: &str, rows: &[PageShotRow], wanted: &[String]) -> Result<()> {
    for (path, png, errors, before, change_bp, report) in rows {
        match before {
            Some((prev_head, prev_png)) => c.execute(
                "INSERT INTO page_shots (preview_id, path, head, png, prev_head, prev_png, change_bp, errors, report) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT (preview_id, path) DO UPDATE SET head = excluded.head, png = excluded.png, prev_head = excluded.prev_head, prev_png = excluded.prev_png, \
                 change_bp = excluded.change_bp, errors = excluded.errors, base_bp = NULL, report = excluded.report, against = NULL",
                params![id, path, head, png, prev_head, prev_png, change_bp, errors, report],
            )?,
            None => c.execute(
                "INSERT INTO page_shots (preview_id, path, head, png, errors, report) VALUES (?, ?, ?, ?, ?, ?) \
                 ON CONFLICT (preview_id, path) DO UPDATE SET head = excluded.head, png = excluded.png, errors = excluded.errors, base_bp = NULL, report = excluded.report, against = NULL",
                params![id, path, head, png, errors, report],
            )?,
        };
    }
    let paths: Vec<String> = c.query("SELECT path FROM page_shots WHERE preview_id = ?", [id], |r| r.get(0))?;
    for gone in paths.iter().filter(|p| !wanted.contains(p)) {
        c.execute("DELETE FROM page_shots WHERE preview_id = ? AND path = ?", [id, gone.as_str()])?;
    }
    Ok(())
}

type ReportCols<'a> = (&'a Option<String>, &'a Option<String>, Option<&'a str>, &'a Option<String>, &'a Option<String>);

fn write_page_report(c: &Conn<'_>, id: &str, (health, a11y, text, links, api): ReportCols<'_>) -> Result<()> {
    c.execute("UPDATE screenshots SET health = ?, a11y = ?, text = ?, links = ?, api = ? WHERE preview_id = ?", params![health, a11y, text, links, api, id])?;
    Ok(())
}

/// [`Store::set_screenshot`]'s row, prepared.
struct ScreenshotRow {
    b64: String,
    now: i64,
    phone_b64: Option<String>,
    speed: Option<String>,
    before: Option<(String, String, Option<String>, Option<String>)>,
    change_bp: Option<i64>,
    phone_change_bp: Option<i64>,
}

fn write_screenshot(c: &Conn<'_>, id: &str, head: &str, row: &ScreenshotRow) -> Result<()> {
    let ScreenshotRow { b64, now, phone_b64, speed, before, change_bp, phone_change_bp } = row;
    let now = *now;
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
    Ok(())
}

/// How long a screenshot link (in a pull request comment) works after the screenshot was taken.
pub const SCREENSHOT_LINK_MS: i64 = 30 * 24 * 3_600_000;

/// A note's id (its review's) as links carry it.
pub fn note_id_ok(note: &str) -> bool {
    !note.is_empty() && note.len() <= 64 && note.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The largest crop of a note's element kept (a PNG).
pub const CROP_MAX: usize = 512 << 10;
/// The most crops kept per preview.
pub const CROPS_KEPT: usize = 50;
/// The most notes cropped in one screenshot run.
pub const CROPS_PER_RUN: usize = 10;

/// The newest activity lines kept per preview.
pub const ACTIVITY_KEEP: i64 = 200;
/// The longest slug: leaves room for a service name in a 63-character host label.
pub const SLUG_MAX: usize = 40;

/// Who may open a preview's services (`runtime`'s gate).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Access {
    /// The link's token (it lets people in only under `access: link`).
    pub token: String,
    /// The project's rule.
    pub mode: crate::settings::Access,
    /// The project's bypass secret's SHA-256 (hex), for automation (`x-roll-preview-bypass`);
    /// never for a fork's (`trusted: false`) preview.
    pub bypass: Option<String>,
}

impl Access {
    /// Whether anything at all is needed to open it (not `public`).
    pub fn required(&self) -> bool {
        self.mode != crate::settings::Access::Public
    }

    /// The link's token, while it lets people in (`access: link`).
    pub fn link_token(&self) -> Option<&str> {
        (self.mode == crate::settings::Access::Link && !self.token.is_empty()).then_some(self.token.as_str())
    }
}

/// The most deploy hooks a project has.
pub const MAX_HOOKS: i64 = 20;

/// A project's deploy hook (Vercel's and Netlify's): a secret URL that rebuilds its previews of
/// `branches` at their heads, for a headless CMS's publish webhook.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Hook {
    pub id: String,
    pub project: String,
    pub name: String,
    pub branches: Vec<String>,
    pub created_at: i64,
    /// When it last rebuilt anything.
    pub fired_at: Option<i64>,
}

fn hook_row(r: &code_sql::Row<'_>) -> code_sql::Result<Hook> {
    let globs: String = r.get(3)?;
    Ok(Hook { id: r.get(0)?, project: r.get(1)?, name: r.get(2)?, branches: serde_json::from_str(&globs).unwrap_or_default(), created_at: r.get(4)?, fired_at: r.get(5)? })
}

/// How many people [`Store::visitors`] names.
pub const VISITORS: usize = 8;

/// A live share link, as [`Store::share_live`] finds it.
pub struct LiveShare {
    pub id: String,
    pub ends: i64,
    /// Who made it (`usr_…`: a person, or the person behind their token); none for the operator.
    pub maker: Option<String>,
    /// When its maker was last asked whether they still may.
    pub maker_checked: i64,
}

/// The SHA-256 (hex) a bypass secret is kept as.
pub fn bypass_hash(secret: &str) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(secret.as_bytes()))
}

/// A signed-in visitor's session ([`Store::visit_session`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisitSession {
    pub user: String,
    /// The roll session (`ses_…`) that got its ticket: the visit lasts no longer than it does.
    pub roll_session: String,
    pub project: String,
    pub expires_at: i64,
    pub checked_at: i64,
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
    links: Option<crate::PageLinks>,
    base: Option<BaseRow>,
}

/// The most caught mail a preview keeps, in bytes (its messages as sent).
pub const MAIL_BYTES: i64 = 10 << 20;

/// How many of a baseline's heads keep their screenshots (to compare changes at their merge base).
pub const BASE_HISTORY: usize = 10;

/// A preview's screenshots, base64 as stored: desktop, phone, and its other pages by path.
/// A preview's schema changes (`None`: not known), their risks, and its release's last run.
type ReleaseFacts = (Option<Vec<String>>, Vec<String>, Option<crate::ReleaseRun>);

struct Shots {
    head: String,
    png: String,
    phone: Option<String>,
    pages: std::collections::BTreeMap<String, String>,
    /// How the page loaded and what it missed, as stored (`PageSpeed`, `PageA11y` JSON).
    speed: Option<String>,
    a11y: Option<String>,
    health: Option<String>,
    /// The first page's visible text.
    text: Option<String>,
    /// Its broken links (`PageLinks` JSON).
    links: Option<String>,
    /// The first service's API operations, a line each.
    api: Option<String>,
    /// The other pages' reports by path (`PageReport` JSON).
    page_reports: std::collections::BTreeMap<String, String>,
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

/// An earlier push's own data: a copy-on-write branch of the preview's branch of `parent`, taken
/// as the next head's data step began, so `revision`'s code runs on the data it left.
#[derive(Clone, Debug, PartialEq)]
pub struct PinnedData {
    pub revision: String,
    pub kind: Kind,
    pub parent: String,
    /// Its name in the product (the guard its delete checks).
    pub name: String,
    pub r#ref: String,
    pub kept: Option<String>,
    /// The preview's branch it was copied from.
    pub source: String,
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
    /// Seals a preview's own variables (rolld: with `ROLL_SECRET_KEY`); unset, they're kept as
    /// written, and read either way.
    pub sealer: std::sync::OnceLock<std::sync::Arc<dyn crate::ports::Sealer>>,
}

const COLUMNS: &str = "id, project_id, slug, repo, change_id, branch, number, title, github_pr, head, trusted, desired, state, error, attempts, \
     next_at, asleep, reset_requested, last_reset_at, last_active_at, expires_at, created_by, created_at, updated_at, generation, runs";

/// What a preview's sealed variables are bound to.
/// What the screenshot links' key is sealed to.
const SIGNING_AAD: &str = "preview-screenshot-links";

fn vars_aad(id: &str) -> String {
    format!("preview-vars:{id}")
}

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
        // Who made a share link (`usr_…`: a person, or the person behind their token; none for the
        // operator), and when it was last asked whether they still may.
        if !db.has_column("shares", "maker")? {
            db.add_column("shares", "maker", "TEXT")?;
        }
        if !db.has_column("shares", "maker_checked")? {
            db.add_column("shares", "maker_checked", "INTEGER NOT NULL DEFAULT 0")?;
        }
        if !db.has_column("previews", "idled")? {
            db.add_column("previews", "idled", "INTEGER NOT NULL DEFAULT 0")?;
        }
        if !db.has_column("previews", "seed_needed")? {
            db.add_column("previews", "seed_needed", "INTEGER NOT NULL DEFAULT 1")?;
        }
        for (col, ty) in [("tried", "TEXT"), ("prev_head", "TEXT"), ("prev_png", "TEXT"), ("change_bp", "INTEGER"), ("speed", "TEXT"), ("prev_speed", "TEXT"), ("health", "TEXT"), ("phone_png", "TEXT"), ("prev_phone_png", "TEXT"), ("phone_change_bp", "INTEGER"), ("a11y", "TEXT"), ("base_preview", "TEXT"), ("base_branch", "TEXT"), ("base_change_bp", "INTEGER"), ("base_phone_bp", "INTEGER"), ("base_head", "TEXT"), ("base_speed", "TEXT"), ("base_a11y", "TEXT"), ("base_health", "TEXT"), ("text", "TEXT"), ("base_text", "TEXT"), ("links", "TEXT"), ("base_links", "TEXT"), ("api", "TEXT"), ("base_api", "TEXT"), ("base_api_breaking", "TEXT"), ("base_api_unread", "INTEGER")] {
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
        // The branch's own schema lines as it was made (JSON), for `schema::diff_since`.
        // What an earlier push's copy of the data holds and costs (read with the branches').
        // And the branch it was copied from (its delete's guard, for products that check it).
        for (col, ty) in [("bytes", "INTEGER NOT NULL DEFAULT 0"), ("usd_month", "REAL NOT NULL DEFAULT 0"), ("spent_usd", "REAL NOT NULL DEFAULT 0"), ("source", "TEXT NOT NULL DEFAULT ''")] {
            if !db.has_column("pinned_data", col)? {
                db.add_column("pinned_data", col, ty)?;
            }
        }
        if !db.has_column("resources", "base_schema")? {
            db.add_column("resources", "base_schema", "TEXT")?;
        }
        for col in ["text", "links", "page_reports", "api"] {
            if !db.has_column("base_history", col)? {
                db.add_column("base_history", col, "TEXT")?;
            }
        }
        for col in ["report", "against"] {
            if !db.has_column("page_shots", col)? {
                db.add_column("page_shots", col, "TEXT")?;
            }
        }
        if !db.has_column("previews", "vars")? {
            db.add_column("previews", "vars", "TEXT")?;
        }
        if !db.has_column("services", "shot_key")? {
            db.add_column("services", "shot_key", "TEXT")?;
        }
        for col in ["kept_until", "goes_at"] {
            if !db.has_column("previews", col)? {
                db.add_column("previews", col, "INTEGER")?;
            }
        }
        if !db.has_column("previews", "rebuilds")? {
            db.add_column("previews", "rebuilds", "INTEGER NOT NULL DEFAULT 0")?;
        }
        for col in ["release_ms", "prev_release_ms"] {
            if !db.has_column("previews", col)? {
                db.add_column("previews", col, "INTEGER")?;
            }
        }
        for col in ["released", "schema_head", "schema_changes", "runs", "ran_head", "teardown_error", "schema_risks", "reported_by"] {
            if !db.has_column("previews", col)? {
                db.add_column("previews", col, "TEXT")?;
            }
        }
        if !db.has_column("visit_tickets", "handle")? {
            db.add_column("visit_tickets", "handle", "TEXT NOT NULL DEFAULT ''")?;
        }
        if !db.has_column("shares", "opened_day")? {
            db.add_column("shares", "opened_day", "INTEGER")?;
        }
        Ok(Store { db, rates: Rates::default(), signing: std::sync::OnceLock::new(), sealer: std::sync::OnceLock::new() })
    }

    pub fn now_ms(&self) -> Result<i64> {
        Ok(self.db.now_ms()?)
    }

    // ── settings ──

    pub fn settings(&self, project: &str) -> Result<Settings> {
        let raw: Option<String> = self.db.read(|c| c.query_row("SELECT json FROM settings WHERE project_id = ?", [project], |r| r.get(0)).optional())?;
        // Only checked settings are stored; a row that doesn't parse (an older version's) is the
        // defaults with previews off, never a guess.
        match raw.and_then(|s| serde_json::from_str::<Settings>(&s).ok()) {
            Some(s) => self.open_rules(project, s),
            None => Ok(Settings::default()),
        }
    }

    /// The rules' variables (`env`, `env_for`) with each pasted value sealed (`v1:…`, bound to
    /// the project, its scope and its name) or, `open`, opened. A reference isn't a secret and
    /// stays as it is. Sealing with no key refuses: a pasted value is never written in the clear.
    fn rules_values(&self, project: &str, mut s: Settings, open: bool) -> Result<Settings> {
        let aad = |scope: &str, name: &str| format!("preview-rules:{project}:{scope}:{name}");
        let each = |scope: &str, vars: &mut std::collections::BTreeMap<String, String>| -> Result<()> {
            for (k, v) in vars.iter_mut() {
                if crate::settings::is_reference(v) {
                    continue;
                }
                if open {
                    if v.starts_with("v1:") {
                        let sealer = self.sealer.get().ok_or_else(|| Error::Internal(format!("{project}'s preview rules hold values sealed with ROLL_SECRET_KEY, which this node doesn't have")))?;
                        *v = sealer.open(&aad(scope, k), v).ok_or_else(|| Error::Internal(format!("{project}'s preview rule {k} doesn't open with this ROLL_SECRET_KEY")))?;
                    }
                } else if !v.starts_with("v1:") {
                    let sealer = self.sealer.get().ok_or_else(|| Error::BadRequest(format!("{scope}: {k}: a value in the preview rules is kept sealed with ROLL_SECRET_KEY, which this node doesn't have: set it, or name the value in roll secrets (roll://ENV/KEY)")))?;
                    *v = sealer.seal(&aad(scope, k), v);
                }
            }
            Ok(())
        };
        each("env", &mut s.env)?;
        for (pattern, vars) in s.env_for.iter_mut() {
            each(&format!("env_for {pattern}"), vars)?;
        }
        Ok(s)
    }

    fn open_rules(&self, project: &str, s: Settings) -> Result<Settings> {
        self.rules_values(project, s, true)
    }

    /// The deployment key, from now on: what's kept in the clear from before (the rules' pasted
    /// values, previews' own variables, the screenshot links' key) is sealed in one write that
    /// leaves nothing of it in the file (`write_erasing`).
    pub fn set_sealer(&self, sealer: std::sync::Arc<dyn crate::ports::Sealer>) -> Result<()> {
        if self.sealer.set(sealer).is_err() {
            return Ok(());
        }
        let settings: Vec<(String, String)> = self.db.read(|c| c.query("SELECT project_id, json FROM settings", (), |r| Ok((r.get(0)?, r.get(1)?))))?;
        let vars: Vec<(String, String)> = self.db.read(|c| c.query("SELECT id, vars FROM previews WHERE vars IS NOT NULL AND vars NOT LIKE 'v1:%'", (), |r| Ok((r.get(0)?, r.get(1)?))))?;
        let mut rules = Vec::new();
        for (project, json) in settings {
            let Ok(s) = serde_json::from_str::<Settings>(&json) else { continue };
            let sealed = serde_json::to_string(&self.rules_values(&project, s, false)?).expect("JSON");
            if sealed != json {
                rules.push((project, sealed));
            }
        }
        let sealer = self.sealer.get().expect("set above").clone();
        let vars: Vec<(String, String)> = vars.into_iter().map(|(id, plain)| (id.clone(), sealer.seal(&vars_aad(&id), &plain))).collect();
        let key: Option<String> = self.db.read(|c| c.query_row("SELECT value FROM keys WHERE name = 'screenshots' AND value NOT LIKE 'v1:%'", (), |r| r.get(0)).optional())?;
        let key = key.map(|k| sealer.seal(SIGNING_AAD, &k));
        if rules.is_empty() && vars.is_empty() && key.is_none() {
            return Ok(());
        }
        self.db.write_erasing(|c| {
            for (project, json) in &rules {
                c.execute("UPDATE settings SET json = ? WHERE project_id = ?", params![json, project])?;
            }
            for (id, sealed) in &vars {
                c.execute("UPDATE previews SET vars = ? WHERE id = ?", params![sealed, id])?;
            }
            if let Some(k) = &key {
                c.execute("UPDATE keys SET value = ? WHERE name = 'screenshots'", [k.as_str()])?;
            }
            Ok::<_, Error>(())
        })
    }

    /// Projects with previews on.
    pub fn enabled_projects(&self) -> Result<Vec<(String, Settings)>> {
        let rows: Vec<(String, String)> = self.db.read(|c| c.query("SELECT project_id, json FROM settings ORDER BY project_id", (), |r| Ok((r.get(0)?, r.get(1)?))))?;
        rows.into_iter().filter_map(|(p, j)| serde_json::from_str::<Settings>(&j).ok().filter(|s| s.enabled).map(|s| (p, s))).map(|(p, s)| Ok((p.clone(), self.open_rules(&p, s)?))).collect()
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

    /// What a service ran when its pages were last taken (its revision, what it's deployed with,
    /// the data's last reset): the same again means its pages can't have changed.
    pub fn service_shot_key(&self, id: &str, name: &str) -> Result<Option<String>> {
        Ok(self.db.read(|c| c.query_row("SELECT shot_key FROM services WHERE preview_id = ? AND name = ?", [id, name], |r| r.get::<_, Option<String>>(0)).optional())?.flatten())
    }

    pub fn set_service_shot_key(&self, id: &str, name: &str, key: &str) -> Result<()> {
        self.db.write(|c| c.execute("UPDATE services SET shot_key = ? WHERE preview_id = ? AND name = ?", params![key, id, name]).map(drop))?;
        Ok(())
    }

    /// A page as it was last kept (its picture, what went wrong while it loaded, its checks), to
    /// keep again at a new head.
    pub fn kept_page(&self, id: &str, path: &str) -> Result<Option<crate::ports::PageShot>> {
        use base64::Engine as _;
        let row: Option<(String, i64, Option<String>)> = self.db.read(|c| c.query_row("SELECT png, errors, report FROM page_shots WHERE preview_id = ? AND path = ?", [id, path], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional())?;
        Ok(row.and_then(|(b64, errors, report)| {
            let png = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
            let report = report.and_then(|r| serde_json::from_str(&r).ok()).unwrap_or_default();
            Some(crate::ports::PageShot { path: path.to_string(), png, errors: errors.max(0) as u32, report })
        }))
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

    /// Who last saved the project's preview rules, when it was a person or their token (the
    /// operator is no one's: `None`): whom a preview's placeholders are made by.
    pub fn set_rules_maker(&self, project: &str, by: Option<&str>) -> Result<()> {
        let now = self.now_ms()?;
        self.db.write(|c| {
            match by {
                Some(b) => c.execute("INSERT INTO rules_makers (project_id, by, at) VALUES (?, ?, ?) ON CONFLICT (project_id) DO UPDATE SET by = excluded.by, at = excluded.at", params![project, b, now])?,
                None => c.execute("DELETE FROM rules_makers WHERE project_id = ?", [project])?,
            };
            Ok::<_, Error>(())
        })
    }

    pub fn rules_maker(&self, project: &str) -> Result<Option<String>> {
        Ok(self.db.read(|c| c.query_row("SELECT by FROM rules_makers WHERE project_id = ?", [project], |r| r.get(0)).optional())?)
    }

    /// Each of a project's references with who wrote it: `(scope, name, reference, by)`. A rule's
    /// scope is `env` or an `env_for` pattern; a preview's own variable's is `preview:<id>`.
    pub fn env_ref_writers(&self, project: &str) -> Result<Vec<(String, String, String, String)>> {
        Ok(self.db.read(|c| c.query("SELECT scope, name, reference, by FROM env_refs WHERE project_id = ?", [project], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))))?)
    }

    /// Who set a preview's own variable to a reference (`Some((reference, by))`), or that it isn't
    /// one any more (`None`).
    pub fn set_var_ref(&self, project: &str, preview: &str, name: &str, set: Option<(&str, &str)>) -> Result<()> {
        let now = self.now_ms()?;
        let scope = format!("preview:{preview}");
        self.db.write(|c| {
            match set {
                Some((reference, by)) => c.execute(
                    "INSERT INTO env_refs (project_id, scope, name, reference, by, at) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT (project_id, scope, name) DO UPDATE SET reference = excluded.reference, by = excluded.by, at = excluded.at",
                    params![project, scope, name, reference, by, now],
                )?,
                None => c.execute("DELETE FROM env_refs WHERE project_id = ? AND scope = ? AND name = ?", params![project, scope, name])?,
            };
            Ok::<_, Error>(())
        })
    }

    /// Who wrote each of a project's rules' references (`(scope, name, reference)`): one kept as
    /// it was keeps its writer, a new or changed one is `by`'s, one gone goes.
    pub fn set_env_refs(&self, project: &str, refs: &[(String, String, String)], by: &str) -> Result<()> {
        let now = self.now_ms()?;
        self.db.write(|c| {
            // The rules' own: a preview's variables' references are their setters' (`set_var_ref`).
            let kept: Vec<(String, String, String)> = c.query("SELECT scope, name, reference FROM env_refs WHERE project_id = ? AND scope NOT LIKE 'preview:%'", [project], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            for k in kept.iter().filter(|k| !refs.contains(k)) {
                c.execute("DELETE FROM env_refs WHERE project_id = ? AND scope = ? AND name = ?", params![project, k.0, k.1])?;
            }
            for (scope, name, reference) in refs.iter().filter(|r| !kept.contains(r)) {
                c.execute(
                    "INSERT INTO env_refs (project_id, scope, name, reference, by, at) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT (project_id, scope, name) DO UPDATE SET reference = excluded.reference, by = excluded.by, at = excluded.at",
                    params![project, scope, name, reference, by, now],
                )?;
            }
            Ok::<_, Error>(())
        })
    }

    pub fn put_settings(&self, project: &str, s: &Settings) -> Result<Settings> {
        s.check()?;
        // Its pasted values sealed (a reference stays as it is); refused without a key.
        let json = serde_json::to_string(&self.rules_values(project, s.clone(), false)?).expect("JSON");
        let now = self.now_ms()?;
        self.db.write(|c| {
            let before: Option<String> = c.query_row("SELECT json FROM settings WHERE project_id = ?", [project], |r| r.get(0)).optional()?;
            let before = before.and_then(|j| serde_json::from_str::<Settings>(&j).ok()).unwrap_or_default();
            c.execute(
                "INSERT INTO settings (project_id, json, updated_at) VALUES (?, ?, ?) ON CONFLICT (project_id) DO UPDATE SET json = excluded.json, updated_at = excluded.updated_at",
                params![project, json, now],
            )?;
            // A change of who may open its previews ends every signed-in visitor's session.
            if before.access != s.access {
                c.execute("DELETE FROM visit_sessions WHERE project_id = ?", [project])?;
                c.execute("DELETE FROM visit_tickets WHERE project_id = ?", [project])?;
            }
            // Leaving link access retires every link handed out: back on `link`, its previews
            // have new ones.
            if before.access == crate::settings::Access::Link && s.access != crate::settings::Access::Link {
                for id in c.query("SELECT id FROM previews WHERE project_id = ?", [project], |r| r.get::<_, String>(0))? {
                    c.execute("UPDATE previews SET token = ? WHERE id = ?", params![new_token(), id])?;
                }
            }
            Ok::<_, Error>(())
        })?;
        Ok(s.clone())
    }

    pub fn delete_settings(&self, project: &str) -> Result<()> {
        self.db.write(|c| {
            c.execute("DELETE FROM settings WHERE project_id = ?", [project])?;
            c.execute("DELETE FROM bypass WHERE project_id = ?", [project])?;
            c.execute("DELETE FROM hooks WHERE project_id = ?", [project])?;
            c.execute("DELETE FROM project_domains WHERE project_id = ?", [project])?;
            c.execute("DELETE FROM git_skips WHERE project_id = ?", [project])?;
            c.execute("DELETE FROM budget_alerts WHERE project_id = ?", [project])?;
            c.execute("DELETE FROM visit_sessions WHERE project_id = ?", [project])?;
            c.execute("DELETE FROM visit_tickets WHERE project_id = ?", [project])?;
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
            emit(c, now, "created", &n.project_id, &id, n.change_id.as_deref(), &serde_json::json!({ "head": n.head }))?;
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
            // What a follower of the events hears: up (ready, or asleep once up) after a push or a
            // first deploy, never a sleeping preview waking; failed, once until it moves.
            let kind = match (old.state, new.state) {
                (State::Creating | State::Updating, State::Ready | State::Sleeping) => Some("ready"),
                (o, State::Failed) if o != State::Failed => Some("failed"),
                _ => None,
            };
            if let Some(kind) = kind {
                emit(c, now, kind, &new.project_id, id, new.change_id.as_deref(), &serde_json::json!({ "head": new.head }))?;
            }
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
        let now = self.now_ms()?;
        self.db.write(|c| {
            let gone = c.query_row("SELECT project_id, change_id, head FROM previews WHERE id = ?", [id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, String>(2)?))).optional()?;
            if let Some((project, change, head)) = gone {
                emit(c, now, "deleted", &project, id, change.as_deref(), &serde_json::json!({ "head": head }))?;
            }
            for t in ["resources", "services", "pinned", "pinned_data", "shares", "visit_tickets", "visit_sessions", "visits", "activity", "screenshots", "page_shots", "visual_page_accepts", "mail", "resolved_notes", "base_history", "leases", "note_crops", "git_pulls", "git_reports", "runtime_facts", "gone_pushes", "previews"] {
                let col = if t == "previews" { "id" } else { "preview_id" };
                c.execute(&format!("DELETE FROM {t} WHERE {col} = ?"), [id])?;
            }
            // Its own variables' references' setters.
            c.execute("DELETE FROM env_refs WHERE scope = ?", [format!("preview:{id}")])?;
            Ok::<_, Error>(())
        })
    }

    /// The events after `after`, oldest first, at most `limit` (`GET /v1/previews/events`).
    pub fn events(&self, after: i64, limit: usize) -> Result<Vec<crate::Event>> {
        Ok(self.db.read(|c| {
            c.query(
                "SELECT seq, at, kind, project_id, preview_id, change_id, data FROM events WHERE seq > ? ORDER BY seq LIMIT ?",
                params![after, limit as i64],
                |r| {
                    let data: String = r.get(6)?;
                    Ok(crate::Event { seq: r.get(0)?, at: r.get(1)?, kind: r.get(2)?, project: r.get(3)?, preview: r.get(4)?, change: r.get(5)?, data: serde_json::from_str(&data).unwrap_or_default() })
                },
            )
        })?)
    }

    /// Record `kind` for preview `id` at `head` unless it's recorded already (`visual_change`:
    /// once a head, however often it's reported).
    pub fn emit_once(&self, id: &str, kind: &str, head: &str) -> Result<()> {
        let now = self.now_ms()?;
        let data = serde_json::json!({ "head": head });
        self.db.write(|c| {
            let Some((project, change)) = c.query_row("SELECT project_id, change_id FROM previews WHERE id = ?", [id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))).optional()? else { return Ok(()) };
            let had: Option<i64> = c.query_row("SELECT seq FROM events WHERE preview_id = ? AND kind = ? AND data = ?", params![id, kind, data.to_string()], |r| r.get(0)).optional()?;
            if had.is_none() {
                emit(c, now, kind, &project, id, change.as_deref(), &data)?;
            }
            Ok::<_, Error>(())
        })
    }

    /// The newest event's seq (0 for none): where a follower that wants only what comes next
    /// starts.
    pub fn latest_event(&self) -> Result<i64> {
        Ok(self.db.read(|c| c.query_row("SELECT COALESCE(MAX(seq), 0) FROM events", (), |r| r.get(0)))?)
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

    /// A branch's schema lines as it was made, if kept ([`crate::schema::diff_since`]).
    pub fn base_schema(&self, id: &str, kind: Kind, parent: &str) -> Result<Option<Vec<String>>> {
        let json: Option<String> = self
            .db
            .read(|c| c.query_row("SELECT base_schema FROM resources WHERE preview_id = ? AND kind = ? AND parent = ?", [id, kind.as_str(), parent], |r| r.get(0)).optional())?
            .flatten();
        Ok(json.and_then(|j| serde_json::from_str(&j).ok()))
    }

    /// Keep a branch's schema lines as it was made, or none (a new branch whose read failed: its
    /// changes are then compared with the parent alone, never with an older branch's lines).
    pub fn set_base_schema(&self, id: &str, kind: Kind, parent: &str, lines: Option<&[String]>) -> Result<()> {
        let json = lines.map(|l| serde_json::to_string(l).unwrap_or_default());
        self.db.write(|c| {
            c.execute("UPDATE resources SET base_schema = ? WHERE preview_id = ? AND kind = ? AND parent = ?", params![json, id, kind.as_str(), parent])?;
            Ok::<_, Error>(())
        })
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
        self.open_vars(id, raw)
    }

    /// A preview's own variables as kept: sealed (`v1:…`, bound to its id) or, from before a key
    /// or without one, plain JSON. Sealed without this node having the key is an error, never an
    /// empty map: its services would start without them.
    fn open_vars(&self, id: &str, raw: Option<String>) -> Result<std::collections::BTreeMap<String, String>> {
        let Some(raw) = raw else { return Ok(Default::default()) };
        let json = if raw.starts_with("v1:") {
            let sealer = self.sealer.get().ok_or_else(|| Error::Internal(format!("{id}'s variables are sealed with ROLL_SECRET_KEY, which this node doesn't have")))?;
            sealer.open(&vars_aad(id), &raw).ok_or_else(|| Error::Internal(format!("{id}'s variables don't open with this ROLL_SECRET_KEY")))?
        } else {
            raw
        };
        Ok(serde_json::from_str(&json).unwrap_or_default())
    }

    /// Change the preview's own variables in one write (`f` may run again; it only edits the map).
    pub fn update_vars(&self, id: &str, f: impl Fn(&mut std::collections::BTreeMap<String, String>) -> Result<()>) -> Result<std::collections::BTreeMap<String, String>> {
        self.db.write(|c| {
            let raw: Option<Option<String>> = c.query_row("SELECT vars FROM previews WHERE id = ?", [id], |r| r.get(0)).optional()?;
            let Some(raw) = raw else { return Err(Error::NotFound(format!("no preview {id}"))) };
            let mut vars = self.open_vars(id, raw)?;
            f(&mut vars)?;
            // Sealed (the plain JSON of an older write goes at this one); with no key, refused:
            // never written in the clear.
            let raw = match (vars.is_empty(), self.sealer.get()) {
                (true, _) => None,
                (false, Some(s)) => Some(s.seal(&vars_aad(id), &serde_json::to_string(&vars).unwrap_or_default())),
                (false, None) => return Err(Error::BadRequest("a preview's own variables are kept sealed with ROLL_SECRET_KEY, which this node doesn't have: set it, or name the value in roll secrets".into())),
            };
            c.execute("UPDATE previews SET vars = ? WHERE id = ?", params![raw, id])?;
            Ok(vars)
        })
    }

    /// When the preview goes unless it's used or kept, once that's close ([`crate::engine`]'s
    /// warning; `None`: not close).
    pub fn goes_at(&self, id: &str) -> Result<Option<i64>> {
        Ok(self.db.read(|c| c.query_row("SELECT goes_at FROM previews WHERE id = ?", [id], |r| r.get(0)).optional())?.flatten())
    }

    /// The name of the project that reports this preview's change (or pull request) when it's
    /// another project's (roll previews' `refresh_voice`); `None`: this one reports it.
    pub fn reported_by(&self, id: &str) -> Result<Option<String>> {
        Ok(self.db.read(|c| c.query_row("SELECT reported_by FROM previews WHERE id = ?", [id], |r| r.get(0)).optional())?.flatten())
    }

    /// Sets [`Store::reported_by`]: whether it changed (written only then).
    pub fn set_reported_by(&self, id: &str, by: Option<&str>) -> Result<bool> {
        if self.reported_by(id)?.as_deref() == by {
            return Ok(false);
        }
        Ok(self.db.write(|c| Ok::<_, Error>(c.execute("UPDATE previews SET reported_by = ? WHERE id = ? AND reported_by IS NOT ?", params![by, id, by])?))? > 0)
    }

    /// The active previews of `repo`'s `branch` in every project (a Git URL service's branch has
    /// one in each project whose canvas holds the service).
    pub fn branch_rows(&self, repo: &str, branch: Option<&str>) -> Result<Vec<Row>> {
        Ok(self.db.read(|c| c.query(&format!("SELECT {COLUMNS} FROM previews WHERE repo = ? AND branch IS ? AND desired = 'active' ORDER BY id"), params![repo, branch], row))?)
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

    /// The release ran at `head` in `ms`; the run before's time is kept to compare.
    pub fn set_released(&self, id: &str, head: &str, ms: i64) -> Result<()> {
        self.db.write(|c| {
            c.execute("UPDATE previews SET released = ?, prev_release_ms = release_ms, release_ms = ? WHERE id = ?", params![head, ms, id])?;
            Ok::<_, Error>(())
        })
    }

    /// The last head a converge ran (its gate passed: trusted, or approved), where the project's
    /// `teardown` runs; `None` once it has, or when nothing of the preview ever ran.
    pub fn ran_head(&self, id: &str) -> Result<Option<String>> {
        Ok(self.db.read(|c| c.query_row("SELECT ran_head FROM previews WHERE id = ?", [id], |r| r.get(0)).optional())?.flatten())
    }

    pub fn set_ran_head(&self, id: &str, head: Option<&str>) -> Result<()> {
        self.db.write(|c| {
            c.execute("UPDATE previews SET ran_head = ? WHERE id = ?", params![head, id])?;
            Ok::<_, Error>(())
        })
    }

    /// What the preview's `teardown` said when it failed, kept until the change is told it went.
    pub fn teardown_error(&self, id: &str) -> Result<Option<String>> {
        Ok(self.db.read(|c| c.query_row("SELECT teardown_error FROM previews WHERE id = ?", [id], |r| r.get(0)).optional())?.flatten())
    }

    pub fn set_teardown_error(&self, id: &str, error: &str) -> Result<()> {
        let error: String = error.chars().take(4000).collect();
        self.db.write(|c| {
            c.execute("UPDATE previews SET teardown_error = ? WHERE id = ?", params![error, id])?;
            Ok::<_, Error>(())
        })
    }

    /// The head the schema's changes were read at.
    pub fn schema_head(&self, id: &str) -> Result<Option<String>> {
        Ok(self.db.read(|c| c.query_row("SELECT schema_head FROM previews WHERE id = ?", [id], |r| r.get(0)).optional())?.flatten())
    }

    /// `risks`: [`crate::schema::risks`] of every line, the ones past `changes`' cap too.
    pub fn set_schema_changes(&self, id: &str, head: &str, changes: Option<&[String]>, risks: &[String]) -> Result<()> {
        let json = changes.map(|c| serde_json::to_string(c).expect("strings serialize"));
        let risks = serde_json::to_string(risks).expect("strings serialize");
        self.db.write(|c| {
            c.execute("UPDATE previews SET schema_head = ?, schema_changes = ?, schema_risks = ? WHERE id = ?", params![head, json, risks, id])?;
            Ok::<_, Error>(())
        })
    }

    /// The head a screenshot was last tried at (taken or not).
    pub fn screenshot_head(&self, id: &str) -> Result<Option<String>> {
        Ok(self.db.read(|c| c.query_row("SELECT tried FROM screenshots WHERE preview_id = ?", [id], |r| r.get(0)).optional())?.flatten())
    }

    pub fn set_screenshot(&self, id: &str, head: &str, png: &[u8], speed: Option<&crate::PageSpeed>, phone: Option<&[u8]>) -> Result<()> {
        let row = self.prepare_screenshot(id, head, png, speed, phone)?;
        self.db.write(|c| write_screenshot(c, id, head, &row))
    }

    /// What [`Store::set_screenshot`] writes, read and compared first (outside the write).
    fn prepare_screenshot(&self, id: &str, head: &str, png: &[u8], speed: Option<&crate::PageSpeed>, phone: Option<&[u8]>) -> Result<ScreenshotRow> {
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
        Ok(ScreenshotRow { b64, now, phone_b64, speed, before, change_bp, phone_change_bp })
    }

    /// What went wrong while the latest screenshot's page loaded (after `set_screenshot`).
    pub fn set_page_report(&self, id: &str, health: Option<&crate::PageHealth>, a11y: Option<&crate::PageA11y>, text: Option<&str>, links: Option<&crate::PageLinks>, api: Option<&[String]>) -> Result<()> {
        let health = health.and_then(|h| serde_json::to_string(h).ok());
        let a11y = a11y.and_then(|a| serde_json::to_string(a).ok());
        let links = links.and_then(|l| serde_json::to_string(l).ok());
        let api = api.map(|ops| ops.join("\n"));
        self.db.write(|c| write_page_report(c, id, (&health, &a11y, text, &links, &api)))
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
        let page_reports = serde_json::to_string(&shots.page_reports).map_err(|e| Error::Internal(e.to_string()))?;
        let now = self.now_ms()?;
        self.db.write(|c| {
            c.execute(
                "INSERT INTO base_history (preview_id, head, png, phone_png, pages, at, speed, a11y, health, text, links, page_reports, api) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT (preview_id, head) DO UPDATE SET png = excluded.png, phone_png = excluded.phone_png, pages = excluded.pages, at = excluded.at, speed = excluded.speed, \
                 a11y = excluded.a11y, health = excluded.health, text = excluded.text, links = excluded.links, page_reports = excluded.page_reports, api = excluded.api",
                params![id, head, shots.png, shots.phone, pages, now, shots.speed, shots.a11y, shots.health, shots.text, shots.links, page_reports, shots.api],
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
        type Raw = (String, String, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>);
        let row: Option<Raw> = self.db.read(|c| {
            c.query_row("SELECT head, png, phone_png, speed, a11y, health, text, links, api FROM screenshots WHERE preview_id = ?", [id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?))
            })
            .optional()
        })?;
        let Some((head, png, phone, speed, a11y, health, text, links, api)) = row else { return Ok(None) };
        let pages: Vec<(String, String, Option<String>)> = self.db.read(|c| c.query("SELECT path, png, report FROM page_shots WHERE preview_id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))))?;
        let page_reports = pages.iter().filter_map(|(p, _, r)| Some((p.clone(), r.clone()?))).collect();
        Ok(Some(Shots { head, png, phone, pages: pages.into_iter().map(|(p, png, _)| (p, png)).collect(), speed, a11y, health, text, links, api, page_reports }))
    }

    /// Baseline `id`'s screenshots at `head`, from its history.
    fn shots_at(&self, id: &str, head: &str) -> Result<Option<Shots>> {
        type Raw = (String, Option<String>, String, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>);
        let row: Option<Raw> = self.db.read(|c| {
            c.query_row("SELECT png, phone_png, pages, speed, a11y, health, text, links, page_reports, api FROM base_history WHERE preview_id = ? AND head = ?", [id, head], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?))
            })
            .optional()
        })?;
        Ok(row.map(|(png, phone, pages, speed, a11y, health, text, links, page_reports, api)| Shots {
            head: head.to_string(),
            png,
            phone,
            pages: serde_json::from_str(&pages).unwrap_or_default(),
            speed,
            a11y,
            health,
            text,
            links,
            api,
            page_reports: page_reports.and_then(|r| serde_json::from_str(&r).ok()).unwrap_or_default(),
        }))
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
        // Each other page's checks against main's page at the same path.
        let report = |m: &std::collections::BTreeMap<String, String>, p: &str| m.get(p).and_then(|r| serde_json::from_str::<crate::PageReport>(r).ok());
        let page_against: Vec<(String, Option<String>)> = ours
            .pages
            .keys()
            .map(|path| {
                let against = report(&ours.page_reports, path).zip(report(&theirs.page_reports, path)).map(|(a, b)| a.against(&b));
                (path.clone(), against.and_then(|a| serde_json::to_string(&a).ok()))
            })
            .collect();
        // The first page's words against the baseline's (Chromatic's DOM snapshots), when both were read.
        let text = ours.text.as_deref().zip(theirs.text.as_deref()).and_then(|(now, then)| serde_json::to_string(&crate::visual::text_diff(then, now)).ok());
        // Its API against the baseline's (`github.openapi`), when both were read: the operations
        // added and removed, and what breaks inside the ones both keep.
        let both = ours.api.as_deref().zip(theirs.api.as_deref());
        let api = both.and_then(|(now, then)| serde_json::to_string(&crate::visual::text_diff(&crate::openapi::operations(then), &crate::openapi::operations(now))).ok());
        let api_breaking = both.and_then(|(now, then)| serde_json::to_string(&crate::openapi::breaking(then, now)).ok());
        // Main's document was read and the change's wasn't (gone, broken, too big, too slow).
        let api_unread = ours.api.is_none() && theirs.api.is_some();
        self.db.write(|c| {
            c.execute(
                "UPDATE screenshots SET base_preview = ?, base_branch = ?, base_change_bp = ?, base_phone_bp = ?, base_head = ?, base_speed = ?, base_a11y = ?, base_health = ?, base_text = ?, \
                 base_links = ?, base_api = ?, base_api_breaking = ?, base_api_unread = ? WHERE preview_id = ?",
                params![base, branch, change, phone, theirs.head, theirs.speed, theirs.a11y, theirs.health, text, theirs.links, api, api_breaking, api_unread, id],
            )?;
            for (path, bp) in &page_bps {
                c.execute("UPDATE page_shots SET base_bp = ? WHERE preview_id = ? AND path = ?", params![bp, id, path])?;
            }
            for (path, against) in &page_against {
                c.execute("UPDATE page_shots SET against = ? WHERE preview_id = ? AND path = ?", params![against, id, path])?;
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
                 base_preview, base_branch, base_change_bp, base_phone_bp, base_head, base_speed, base_a11y, base_health, base_text, links, base_links, base_api, base_api_breaking, base_api_unread FROM screenshots WHERE preview_id = ?",
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
                            text: json(r.get(18)?),
                            links: json(r.get(20)?),
                            api: json(r.get(21)?),
                            api_breaking: json(r.get(22)?).unwrap_or_default(),
                            api_unread: r.get::<_, Option<bool>>(23)?.unwrap_or(false),
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
                        links: json(r.get(19)?),
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
        let rows = self.prepare_page_shots(id, head, pages)?;
        self.db.write(|c| write_page_shots(c, id, head, &rows, wanted))
    }

    /// What [`Store::set_page_shots`] writes, read and compared first (outside the write).
    fn prepare_page_shots(&self, id: &str, head: &str, pages: &[crate::ports::PageShot]) -> Result<Vec<PageShotRow>> {
        use base64::Engine as _;
        let b64e = base64::engine::general_purpose::STANDARD;
        let mut rows = Vec::new();
        for p in pages {
            let before: Option<(String, String)> = self.db.read(|c| c.query_row("SELECT head, png FROM page_shots WHERE preview_id = ? AND path = ?", [id, &p.path], |r| Ok((r.get(0)?, r.get(1)?))).optional())?;
            let before = before.filter(|(h, _)| h != head);
            let change_bp = before.as_ref().and_then(|(_, old)| crate::visual::changed(&b64e.decode(old).ok()?, &p.png)).map(|f| (f * 10_000.0).round() as i64);
            let report = serde_json::to_string(&p.report).ok();
            rows.push((p.path.clone(), b64e.encode(&p.png), p.errors as i64, before, change_bp, report));
        }
        Ok(rows)
    }

    /// A screenshot run kept whole, in one transaction: the first page's picture, its checks and
    /// the other pages. Anything comparing against it (a change against main's baseline, main's
    /// baseline against waiting changes) sees all of it or none: kept in three writes, a
    /// comparison could see the picture before its checks and pages (screenshots run side by
    /// side) and keep that half for good.
    pub fn keep_shot(&self, id: &str, head: &str, shot: &crate::ports::Shot, pages: &[crate::ports::PageShot], wanted: &[String]) -> Result<()> {
        let row = self.prepare_screenshot(id, head, &shot.png, shot.speed.as_ref(), shot.phone.as_deref())?;
        let page_rows = self.prepare_page_shots(id, head, pages)?;
        let health = shot.health.as_ref().and_then(|h| serde_json::to_string(h).ok());
        let a11y = shot.a11y.as_ref().and_then(|a| serde_json::to_string(a).ok());
        let links = shot.links.as_ref().and_then(|l| serde_json::to_string(l).ok());
        let api = shot.api.as_ref().map(|ops| ops.join("\n"));
        self.db.write(|c| {
            write_screenshot(c, id, head, &row)?;
            write_page_report(c, id, (&health, &a11y, shot.text.as_deref(), &links, &api))?;
            write_page_shots(c, id, head, &page_rows, wanted)
        })
    }

    /// The other pages' screenshots, by path: how each changed and compares with the baseline's.
    pub fn page_checks(&self, id: &str) -> Result<Vec<crate::PageCheck>> {
        type Raw = (String, i64, Option<String>, Option<i64>, Option<i64>, Option<String>, Option<String>);
        let rows: Vec<Raw> = self.db.read(|c| {
            c.query("SELECT path, errors, prev_head, change_bp, base_bp, report, against FROM page_shots WHERE preview_id = ? ORDER BY path", [id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
            })
        })?;
        Ok(rows
            .into_iter()
            .map(|(path, errors, since, change, base, report, against)| {
                let report: crate::PageReport = report.and_then(|r| serde_json::from_str(&r).ok()).unwrap_or_default();
                (path, errors, since, change, base, report, against.and_then(|a| serde_json::from_str(&a).ok()))
            })
            .map(|(path, errors, since, change, base, report, against)| crate::PageCheck {
                path,
                errors: errors as u32,
                since: since.filter(|_| change.is_some()),
                percent: change.map(|bp| bp as f64 / 100.0),
                base_percent: base.map(|bp| bp as f64 / 100.0),
                a11y: report.a11y,
                links: report.links,
                against,
                unstable: report.unstable,
                step: report.step,
            })
            .collect())
    }

    /// `by` accepted how the preview's pages look at the latest screenshot's head (`None`: no
    /// screenshot to accept): `page` only (`""` the first page, else another page's path), or
    /// every page taken there.
    /// `seen`: the head whose screenshots were looked at; refused (`Conflict`) when a newer one
    /// came in meanwhile, so nobody accepts pages they didn't see.
    pub fn accept_visual(&self, id: &str, by: &str, seen: Option<&str>, page: Option<&str>) -> Result<Option<String>> {
        let now = self.now_ms()?;
        // Read and written in one transaction: a screenshot landing in between can't be accepted.
        self.db.write(|c| {
            let head: Option<String> = c.query_row("SELECT head FROM screenshots WHERE preview_id = ?", [id], |r| r.get(0)).optional()?;
            let Some(head) = head.filter(|h| !h.is_empty()) else { return Ok(None) };
            if seen.is_some_and(|s| s != head) {
                return Err(Error::Conflict(format!("a newer screenshot came in (at {}): look at it first", head.chars().take(12).collect::<String>())));
            }
            let others: Vec<String> = c.query("SELECT path FROM page_shots WHERE preview_id = ?", [id], |r| r.get(0))?;
            let pages: Vec<String> = match page {
                None => std::iter::once(String::new()).chain(others).collect(),
                Some(p) if p.is_empty() || others.iter().any(|o| o == p) => vec![p.to_string()],
                Some(p) => return Err(Error::NotFound(format!("the preview has no screenshot of {p:?}"))),
            };
            for path in pages {
                c.execute(
                    "INSERT INTO visual_page_accepts (preview_id, path, head, by, at) VALUES (?, ?, ?, ?, ?) ON CONFLICT (preview_id, path) DO UPDATE SET head = excluded.head, by = excluded.by, at = excluded.at",
                    params![id, path, head, by, now],
                )?;
            }
            Ok::<_, Error>(Some(head))
        })
    }

    /// After a screenshot: each page accepted at the head just before is accepted at this one when
    /// it looks exactly as it did there (Chromatic carries accepted baselines forward), so a push
    /// that changes nothing visible isn't reviewed again, and one that changes a page asks only
    /// about that page. `true` when an acceptance moved.
    /// `threshold`: the project's `github.visual_threshold`; a page that changed less is as it was.
    pub fn carry_acceptance(&self, id: &str, threshold: f64) -> Result<bool> {
        let as_was = move |bp: Option<i64>| bp.is_some_and(|bp| bp as f64 / 100.0 <= threshold);
        self.db.write(|c| {
            // (head, the head before, desktop change, phone change)
            type Shot = (String, Option<String>, Option<i64>, Option<i64>);
            let shot: Option<Shot> =
                c.query_row("SELECT head, prev_head, change_bp, phone_change_bp FROM screenshots WHERE preview_id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional()?;
            let Some((head, Some(prev), desktop, phone)) = shot else { return Ok(false) };
            // Each page's (path, the head before, change): the first page's at both widths.
            let mut same: Vec<String> = Vec::new();
            if as_was(desktop) && (phone.is_none() || as_was(phone)) {
                same.push(String::new());
            }
            let pages: Vec<(String, Option<String>, Option<i64>)> = c.query("SELECT path, prev_head, change_bp FROM page_shots WHERE preview_id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            same.extend(pages.into_iter().filter(|(_, h, bp)| h.as_deref() == Some(prev.as_str()) && as_was(*bp)).map(|(p, _, _)| p));
            let mut moved = false;
            for path in same {
                moved |= c.execute("UPDATE visual_page_accepts SET head = ? WHERE preview_id = ? AND path = ? AND head = ?", [head.as_str(), id, path.as_str(), prev.as_str()])? > 0;
            }
            Ok::<_, Error>(moved)
        })
    }

    /// The visual review of a change's preview (`github.visual_review`), once its pages were
    /// compared with the baseline's: each page that differs, who accepted it at this head, and
    /// the change accepted once every one of them is.
    /// `first_unstable`: the first page moves by itself (its review says so).
    #[allow(clippy::too_many_arguments)]
    fn visual_review(&self, r: &Row, rule: crate::settings::VisualReviewRule, threshold: f64, base: Option<&crate::BaseChange>, pages: &[crate::PageCheck], first_unstable: bool, shot: Option<String>) -> Result<Option<crate::VisualReview>> {
        if r.change_id.is_none() || rule == crate::settings::VisualReviewRule::Off {
            return Ok(None);
        }
        let Some(base) = base else { return Ok(None) };
        let accepts: Vec<(String, String, String, i64)> = self.db.read(|c| c.query("SELECT path, head, by, at FROM visual_page_accepts WHERE preview_id = ?", [&r.id], |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?, x.get(3)?))))?;
        let accepted = |path: &str| accepts.iter().find(|(p, h, _, _)| p == path && shot.as_deref() == Some(h.as_str())).map(|(_, _, by, at)| (by.clone(), *at));
        let first = base.percent.max(base.phone_percent.unwrap_or(0.0));
        let changed: Vec<(String, f64)> =
            std::iter::once((String::new(), first)).chain(pages.iter().filter_map(|p| Some((p.path.clone(), p.base_percent?)))).filter(|(_, pct)| *pct > threshold).collect();
        let reviews: Vec<crate::PageReview> = changed
            .iter()
            .map(|(path, percent)| {
                let a = accepted(path);
                let unstable = if path.is_empty() { first_unstable } else { pages.iter().any(|p| p.path == *path && p.unstable) };
                crate::PageReview { path: path.clone(), percent: *percent, by: a.as_ref().map(|(by, _)| by.clone()), at: a.map(|(_, at)| at), unstable }
            })
            .collect();
        let percent = changed.iter().map(|(_, p)| *p).fold(0.0_f64, f64::max);
        let state = if reviews.is_empty() {
            crate::VisualState::Unchanged
        } else if reviews.iter().all(|p| p.by.is_some()) {
            crate::VisualState::Accepted
        } else {
            crate::VisualState::Changed
        };
        // Who accepted it: whoever accepted its last page.
        let last = reviews.iter().filter(|_| state == crate::VisualState::Accepted).max_by_key(|p| p.at);
        let (by, at) = (last.and_then(|p| p.by.clone()), last.and_then(|p| p.at));
        Ok(Some(crate::VisualReview { state, branch: base.branch.clone(), percent, by, at, required: rule == crate::settings::VisualReviewRule::Required, head: shot.unwrap_or_default(), pages: reviews }))
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
    /// The password of the preview's SMTP sign-in (`SMTP_PASSWORD`, its id the user): a keyed
    /// hash only roll makes, so nothing needs storing and nobody else's code can send as it.
    pub fn mail_password(&self, id: &str) -> Result<String> {
        use hmac::Mac;
        let mut m = hmac::Hmac::<sha2::Sha256>::new_from_slice(&self.signing_key()?).map_err(|e| Error::Internal(e.to_string()))?;
        m.update(b"smtp:");
        m.update(id.as_bytes());
        Ok(hex::encode(&m.finalize().into_bytes()[..16]))
    }

    /// The token in a preview's OpenTelemetry intake's path (`…/otlp/{token}`), derived as its SMTP
    /// password is (nothing kept; a new signing key changes them all).
    pub fn trace_token(&self, id: &str) -> Result<String> {
        use hmac::Mac;
        let mut m = hmac::Hmac::<sha2::Sha256>::new_from_slice(&self.signing_key()?).map_err(|e| Error::Internal(e.to_string()))?;
        m.update(b"otlp:");
        m.update(id.as_bytes());
        Ok(hex::encode(&m.finalize().into_bytes()[..16]))
    }

    /// Whether `user`/`password` are a live preview's SMTP sign-in (compared in constant time).
    pub fn mail_signs_in(&self, user: &str, password: &str) -> Result<bool> {
        let Ok(r) = self.row(user) else { return Ok(false) };
        if r.desired == crate::Desired::Deleted {
            return Ok(false);
        }
        Ok(crate::runtime::same(&self.mail_password(&r.id)?, password))
    }

    /// `by` resolved a note left on the preview's page (its review's id): the page stops listing
    /// it. Resolving one already resolved changes nothing.
    pub fn resolve_note(&self, preview: &str, review: &str, by: &str) -> Result<()> {
        let now = self.now_ms()?;
        self.db.write(|c| {
            c.execute("INSERT INTO resolved_notes (preview_id, review_id, by, at) VALUES (?, ?, ?, ?) ON CONFLICT (preview_id, review_id) DO NOTHING", params![preview, review, by, now])?;
            Ok::<_, Error>(())
        })
    }

    /// The notes resolved on a preview's page (their reviews' ids).
    pub fn resolved_notes(&self, preview: &str) -> Result<std::collections::HashSet<String>> {
        let ids: Vec<String> = self.db.read(|c| c.query("SELECT review_id FROM resolved_notes WHERE preview_id = ?", [preview], |r| r.get(0)))?;
        Ok(ids.into_iter().collect())
    }

    /// A caught message, kept with its preview: its last 50, at most [`MAIL_BYTES`] of them in
    /// all (the oldest go first), so a preview's code can't fill the store.
    pub fn keep_mail(&self, preview: &str, mail: &crate::mail::Mail) -> Result<()> {
        use base64::Engine as _;
        let now = self.now_ms()?;
        let id = format!("ml_{}", &new_token()[..16]);
        let to = serde_json::to_string(&mail.to).unwrap_or_else(|_| "[]".into());
        let raw = base64::engine::general_purpose::STANDARD.encode(&mail.raw);
        self.db.write(|c| {
            c.execute(
                "INSERT INTO mail (id, preview_id, at, sender, recipients, subject, size, raw) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                params![id, preview, now, mail.from, to, mail.subject(), mail.raw.len() as i64, raw],
            )?;
            c.execute(
                "DELETE FROM mail WHERE preview_id = ? AND id NOT IN (SELECT id FROM mail WHERE preview_id = ? ORDER BY at DESC, id DESC LIMIT 50)",
                params![preview, preview],
            )?;
            let kept: Vec<(String, i64)> = c.query("SELECT id, size FROM mail WHERE preview_id = ? ORDER BY at DESC, id DESC", [preview], |r| Ok((r.get(0)?, r.get(1)?)))?;
            let mut total = 0i64;
            for (mid, size) in kept {
                total += size;
                if total > MAIL_BYTES {
                    c.execute("DELETE FROM mail WHERE id = ?", [mid.as_str()])?;
                }
            }
            Ok::<_, Error>(())
        })
    }

    /// The preview's caught mail, newest first (without the messages).
    pub fn mails(&self, preview: &str) -> Result<Vec<crate::CaughtMail>> {
        type Raw = (String, i64, String, String, Option<String>, i64);
        let rows: Vec<Raw> = self.db.read(|c| {
            c.query("SELECT id, at, sender, recipients, subject, size FROM mail WHERE preview_id = ? ORDER BY at DESC, id DESC", [preview], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
            })
        })?;
        Ok(rows
            .into_iter()
            .map(|(id, at, from, to, subject, size)| crate::CaughtMail { id, at, from, to: serde_json::from_str(&to).unwrap_or_default(), subject, size })
            .collect())
    }

    /// One caught message as it was sent.
    pub fn mail_raw(&self, preview: &str, id: &str) -> Result<Option<Vec<u8>>> {
        use base64::Engine as _;
        let raw: Option<String> = self.db.read(|c| c.query_row("SELECT raw FROM mail WHERE preview_id = ? AND id = ?", [preview, id], |r| r.get(0)).optional())?;
        Ok(raw.and_then(|r| base64::engine::general_purpose::STANDARD.decode(r).ok()))
    }

    /// The screenshot links' key: kept sealed with the deployment key, shared by the fleet; with no
    /// key, this process's own, never written (its links stop at a restart: local development).
    fn signing_key(&self) -> Result<Vec<u8>> {
        if let Some(k) = self.signing.get() {
            return Ok(k.clone());
        }
        let mut raw = [0u8; 32];
        getrandom::fill(&mut raw).expect("system randomness");
        let fresh = hex::encode(raw);
        let Some(sealer) = self.sealer.get() else {
            // A key from before in the clear goes.
            self.db.write_erasing(|c| c.execute("DELETE FROM keys WHERE name = 'screenshots' AND value NOT LIKE 'v1:%'", ()).map(drop).map_err(Error::from))?;
            return Ok(self.signing.get_or_init(|| raw.to_vec()).clone());
        };
        let sealed = sealer.seal(SIGNING_AAD, &fresh);
        self.db.write(|c| {
            c.execute("INSERT INTO keys (name, value) VALUES ('screenshots', ?) ON CONFLICT (name) DO NOTHING", [sealed.as_str()])?;
            Ok::<_, Error>(())
        })?;
        let v: String = self.db.read(|c| c.query_row("SELECT value FROM keys WHERE name = 'screenshots'", (), |r| r.get(0)))?;
        let v = if v.starts_with("v1:") { sealer.open(SIGNING_AAD, &v).ok_or_else(|| Error::Internal("the screenshot links' key doesn't open with this ROLL_SECRET_KEY".into()))? } else { v };
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

    /// The same for a note's crop (`note=<id>`): its own signature per note.
    pub fn note_crop_link(&self, id: &str, note: &str) -> Result<Option<(i64, String)>> {
        if !note_id_ok(note) {
            return Ok(None);
        }
        self.link(id, Some(&format!("note={note}")))
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
            c.execute("UPDATE previews SET schema_changes = NULL, schema_risks = NULL WHERE id = ? AND schema_changes IS NOT NULL", [id])?;
            Ok::<_, Error>(())
        })
    }

    /// What the release last did, in one read: the schema's changes and how long it took.
    /// The schema's changes and their risks, and the release's run.
    fn release_facts(&self, id: &str) -> Result<ReleaseFacts> {
        type Raw = (Option<String>, Option<String>, Option<String>, Option<i64>, Option<i64>);
        let raw: Option<Raw> = self.db.read(|c| {
            c.query_row("SELECT schema_changes, schema_risks, released, release_ms, prev_release_ms FROM previews WHERE id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).optional()
        })?;
        let Some((changes, risks, head, ms, before)) = raw else { return Ok((None, Vec::new(), None)) };
        let run = head.zip(ms).map(|(head, ms)| crate::ReleaseRun { head, ms, before_ms: before });
        Ok((changes.and_then(|j| serde_json::from_str(&j).ok()), risks.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default(), run))
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
        let mode = self.settings(&project)?.access;
        // A fork's preview never opens to the project's secret: CI that visits it would hand the
        // secret to the fork's code.
        Ok(Some(Access { token: token.unwrap_or_default(), mode, bypass: bypass.filter(|_| trusted) }))
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

    // ── deploy hooks: a secret URL that rebuilds a project's previews of some branches ──

    /// A new hook for `project`'s previews of `branches` (globs): its secret, returned once and
    /// kept only as its SHA-256 (`rlb_…`, a roll secret leak scanning finds).
    pub fn create_hook(&self, project: &str, name: &str, branches: &[String]) -> Result<(Hook, String)> {
        let secret = roll_auth::credentials::new_secret("rlb_");
        let (id, hash, now) = (format!("hk_{}", crate::new_id()), bypass_hash(&secret), self.now_ms()?);
        let globs = serde_json::to_string(branches).map_err(|e| Error::Internal(e.to_string()))?;
        self.db.write(|c| {
            let n: i64 = c.query_row("SELECT COUNT(*) FROM hooks WHERE project_id = ?", [project], |r| r.get(0))?;
            if n >= MAX_HOOKS {
                return Err(Error::Conflict(format!("the project has {MAX_HOOKS} deploy hooks: delete one first")));
            }
            c.execute("INSERT INTO hooks (id, project_id, name, branches, hash, created_at) VALUES (?, ?, ?, ?, ?, ?)", params![id, project, name, globs, hash, now])?;
            Ok(())
        })?;
        Ok((Hook { id, project: project.to_string(), name: name.to_string(), branches: branches.to_vec(), created_at: now, fired_at: None }, secret))
    }

    /// `project`'s hooks (never their secrets).
    pub fn hooks(&self, project: &str) -> Result<Vec<Hook>> {
        Ok(self.db.read(|c| c.query("SELECT id, project_id, name, branches, created_at, fired_at FROM hooks WHERE project_id = ? ORDER BY created_at, id", [project], hook_row))?)
    }

    pub fn delete_hook(&self, project: &str, id: &str) -> Result<()> {
        let n = self.db.write(|c| Ok::<_, Error>(c.execute("DELETE FROM hooks WHERE project_id = ? AND id = ?", [project, id])?))?;
        if n == 0 {
            return Err(Error::NotFound("no such deploy hook".into()));
        }
        Ok(())
    }

    /// The hook a secret is, if it's a live one.
    pub fn hook_by_secret(&self, secret: &str) -> Result<Option<Hook>> {
        let hash = bypass_hash(secret);
        Ok(self.db.read(|c| c.query_row("SELECT id, project_id, name, branches, created_at, fired_at FROM hooks WHERE hash = ?", [hash.as_str()], hook_row).optional())?)
    }

    /// Records a firing at `now` unless the hook fired less than `every` ms ago: whether it
    /// counts (a CMS that saves often makes one rebuild a minute, not one a save).
    pub fn fire_hook(&self, id: &str, now: i64, every: i64) -> Result<bool> {
        Ok(self.db.write(|c| Ok::<_, Error>(c.execute("UPDATE hooks SET fired_at = ?1 WHERE id = ?2 AND (fired_at IS NULL OR fired_at <= ?1 - ?3)", params![now, id, every])?))? > 0)
    }

    /// One more rebuild asked of a preview (a deploy hook): its services are built again at their
    /// heads, with nothing cached.
    pub fn add_rebuild(&self, id: &str) -> Result<()> {
        self.db.write(|c| Ok::<_, Error>(c.execute("UPDATE previews SET rebuilds = rebuilds + 1, next_at = 0, generation = generation + 1 WHERE id = ?", [id])?))?;
        Ok(())
    }

    /// How many rebuilds were asked of a preview.
    pub fn rebuilds(&self, id: &str) -> Result<i64> {
        Ok(self.db.read(|c| c.query_row("SELECT rebuilds FROM previews WHERE id = ?", [id], |r| r.get(0)).optional())?.unwrap_or(0))
    }

    /// A Git URL service's branch whose pull request's opener the project skips (`skip_authors`):
    /// its pushes make no preview until the pull request closes, the branch goes or a title asks.
    pub fn set_git_skip(&self, project: &str, repo: &str, branch: &str, on: bool) -> Result<()> {
        self.db.write(|c| {
            if on {
                c.execute("INSERT INTO git_skips (project_id, repo, branch) VALUES (?, ?, ?) ON CONFLICT DO NOTHING", [project, repo, branch])?;
            } else {
                c.execute("DELETE FROM git_skips WHERE project_id = ? AND repo = ? AND branch = ?", [project, repo, branch])?;
            }
            Ok::<_, Error>(())
        })
    }

    pub fn git_skipped(&self, project: &str, repo: &str, branch: &str) -> Result<bool> {
        Ok(self.db.read(|c| c.query_row("SELECT 1 FROM git_skips WHERE project_id = ? AND repo = ? AND branch = ?", [project, repo, branch], |_| Ok(())).optional())?.is_some())
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
    /// The project's own preview domain as last read from roll apps (`(base, https)`).
    pub fn project_domain(&self, project: &str) -> Result<Option<(String, bool)>> {
        Ok(self.db.read(|c| c.query_row("SELECT base, https FROM project_domains WHERE project_id = ?", [project], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? != 0))).optional())?)
    }

    /// Keeps `project`'s own preview domain: whether it changed. Its runtime services' URLs move
    /// with it (`url`: a host label's URL under the domain now in force; `old_url`: under the one
    /// before); a URL roll apps chose isn't previews' to move (apps says its own at each deploy).
    pub fn set_project_domain(&self, project: &str, domain: Option<(&str, bool)>, url: &dyn Fn(&str) -> String, old_url: &dyn Fn(&str) -> String) -> Result<bool> {
        if self.project_domain(project)?.as_ref().map(|(b, h)| (b.as_str(), *h)) == domain {
            return Ok(false);
        }
        self.db.write(|c| {
            match domain {
                // roll apps verified one owner of a base: another project's row for it is stale.
                Some((base, https)) => {
                    c.execute("DELETE FROM project_domains WHERE base = ? AND project_id <> ?", [base, project])?;
                    c.execute(
                        "INSERT INTO project_domains (project_id, base, https) VALUES (?, ?, ?) ON CONFLICT (project_id) DO UPDATE SET base = excluded.base, https = excluded.https",
                        params![project, base, https as i64],
                    )?;
                }
                None => {
                    c.execute("DELETE FROM project_domains WHERE project_id = ?", [project])?;
                }
            }
            let hosts: Vec<(String, String, String, String)> = c.query(
                "SELECT s.preview_id, s.name, s.host, s.url FROM services s JOIN previews p ON p.id = s.preview_id WHERE p.project_id = ?",
                [project],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;
            for (preview, name, host, was) in hosts {
                if was == old_url(&host) {
                    c.execute("UPDATE services SET url = ? WHERE preview_id = ? AND name = ?", params![url(&host), preview, name])?;
                }
            }
            Ok::<_, Error>(())
        })?;
        Ok(true)
    }

    /// Every project's own preview domain's base.
    pub fn domain_bases(&self) -> Result<Vec<String>> {
        Ok(self.db.read(|c| c.query("SELECT base FROM project_domains", (), |r| r.get(0)))?)
    }

    /// The preview and service `<label>.<base>` names under a project's own preview domain: only
    /// a preview of the project that holds the domain.
    pub fn service_for_domain_host(&self, label: &str, base: &str) -> Result<Option<(String, String)>> {
        let Some(project) = self.db.read(|c| c.query_row("SELECT project_id FROM project_domains WHERE base = ?", [base], |r| r.get::<_, String>(0)).optional())? else { return Ok(None) };
        let Some((preview, service)) = self.service_for_host(label)? else { return Ok(None) };
        let theirs = self.db.read(|c| c.query_row("SELECT project_id FROM previews WHERE id = ?", [preview.as_str()], |r| r.get::<_, String>(0)).optional())?;
        Ok((theirs.as_deref() == Some(project.as_str())).then_some((preview, service)))
    }

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

    /// An earlier push no longer kept, by the host it had (a label).
    pub fn gone_push(&self, host: &str) -> Result<Option<GonePush>> {
        Ok(self.db.read(|c| {
            c.query_row(
                "SELECT g.preview_id, g.service, g.revision, s.url FROM gone_pushes g LEFT JOIN services s ON s.preview_id = g.preview_id AND s.name = g.service WHERE g.host = ?",
                [host],
                |r| Ok(GonePush { preview: r.get(0)?, service: r.get(1)?, revision: r.get(2)?, current_url: r.get(3)? }),
            )
            .optional()
        })?)
    }

    /// [`Store::gone_push`] under a project's own preview domain (`base`): only that project's.
    pub fn gone_domain_push(&self, label: &str, base: &str) -> Result<Option<GonePush>> {
        let Some(project) = self.db.read(|c| c.query_row("SELECT project_id FROM project_domains WHERE base = ?", [base], |r| r.get::<_, String>(0)).optional())? else { return Ok(None) };
        let Some(g) = self.gone_push(label)? else { return Ok(None) };
        let theirs = self.db.read(|c| c.query_row("SELECT project_id FROM previews WHERE id = ?", [g.preview.as_str()], |r| r.get::<_, String>(0)).optional())?;
        Ok((theirs.as_deref() == Some(project.as_str())).then_some(g))
    }

    /// The host a service (or an earlier push's, `<service>@<revision>`) answers on.
    pub fn service_host(&self, id: &str, name: &str) -> Result<Option<String>> {
        Ok(self.db.read(|c| match name.split_once('@') {
            Some((svc, rev)) => c.query_row("SELECT host FROM pinned WHERE preview_id = ? AND service = ? AND revision = ?", [id, svc, rev], |r| r.get(0)).optional(),
            None => c.query_row("SELECT host FROM services WHERE preview_id = ? AND name = ?", [id, name], |r| r.get(0)).optional(),
        })?)
    }

    /// The data kept for earlier pushes: a snapshot of a branch per head ([`PinnedData`]).
    pub fn pinned_data(&self, id: &str) -> Result<Vec<PinnedData>> {
        Ok(self.db.read(|c| {
            c.query("SELECT revision, kind, parent, name, ref, kept, source FROM pinned_data WHERE preview_id = ? ORDER BY at", [id], |r| {
                let kind: String = r.get(1)?;
                Ok(PinnedData { revision: r.get(0)?, kind: Kind::parse(&kind).unwrap_or(Kind::Postgres), parent: r.get(2)?, name: r.get(3)?, r#ref: r.get(4)?, kept: r.get(5)?, source: r.get(6)? })
            })
        })?)
    }

    pub fn add_pinned_data(&self, id: &str, d: &PinnedData) -> Result<()> {
        let now = self.now_ms()?;
        self.db.write(|c| {
            c.execute(
                "INSERT INTO pinned_data (preview_id, revision, kind, parent, name, ref, kept, at, source) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT (preview_id, revision, kind, parent) DO UPDATE SET name = excluded.name, ref = excluded.ref, kept = excluded.kept, source = excluded.source",
                params![id, d.revision, d.kind.as_str(), d.parent, d.name, d.r#ref, d.kept, now, d.source],
            )?;
            Ok::<_, Error>(())
        })
    }

    /// What an earlier push's copy holds and costs, as its product last said.
    pub fn set_pinned_usage(&self, id: &str, d: &PinnedData, u: &crate::ports::Usage) -> Result<()> {
        self.db.write(|c| {
            c.execute(
                "UPDATE pinned_data SET bytes = ?, usd_month = ?, spent_usd = ? WHERE preview_id = ? AND revision = ? AND kind = ? AND parent = ?",
                params![u.bytes, u.storage_usd_month, u.month_to_date_usd, id, d.revision, d.kind.as_str(), d.parent],
            )?;
            Ok::<_, Error>(())
        })
    }

    /// The earlier pushes' copies' `(bytes, usd a month kept, billed this month)`.
    pub fn pinned_usage(&self, id: &str) -> Result<Vec<(i64, f64, f64)>> {
        Ok(self.db.read(|c| c.query("SELECT bytes, usd_month, spent_usd FROM pinned_data WHERE preview_id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))))?)
    }

    pub fn drop_pinned_data(&self, id: &str, d: &PinnedData) -> Result<()> {
        self.db.write(|c| {
            c.execute("DELETE FROM pinned_data WHERE preview_id = ? AND revision = ? AND kind = ? AND parent = ?", params![id, d.revision, d.kind.as_str(), d.parent])?;
            Ok::<_, Error>(())
        })
    }

    /// Keep `revision` of `service` reachable at a host of its own (an earlier push), and the
    /// [`PINNED_KEEP`] latest per service. Returns the ones that no longer fit, by
    /// `<service>@<revision>`, whose containers go.
    pub fn pin(&self, id: &str, service: &str, revision: &str, url: &dyn Fn(&str) -> String) -> Result<Vec<String>> {
        self.pin_at(id, service, revision, None, url)
    }

    /// [`Store::pin`] for a build its backend keeps at a URL of its own (roll apps' deployment
    /// URL): nothing of roll previews' answers for it. Its host is kept as `apps:<host>`, which no
    /// DNS label can be, so roll previews' proxy never takes a request as its.
    pub fn pin_build(&self, id: &str, service: &str, revision: &str, own_url: &str) -> Result<Vec<String>> {
        self.pin_at(id, service, revision, Some(own_url), &|_| String::new())
    }

    fn pin_at(&self, id: &str, service: &str, revision: &str, own_url: Option<&str>, url: &dyn Fn(&str) -> String) -> Result<Vec<String>> {
        let now = self.now_ms()?;
        self.db.write(|c| {
            let (host, url) = match own_url {
                Some(u) => (format!("apps:{}", u.split("://").last().unwrap_or(u).split('/').next().unwrap_or(u)), u.to_string()),
                None => {
                    let slug: String = c.query_row("SELECT slug FROM previews WHERE id = ?", [id], |r| r.get(0))?;
                    let short: String = revision.strip_prefix("cs_").unwrap_or(revision).chars().take(10).collect();
                    let mut host = crate::label_part(&format!("{service}-{short}-{slug}"), 63);
                    if host_taken(c, &host)? {
                        host = format!("{}-{}", crate::label_part(&host, 58), &id[id.len() - 4..]);
                    }
                    let u = url(&host);
                    (host, u)
                }
            };
            c.execute(
                "INSERT INTO pinned (preview_id, service, revision, host, url, at) VALUES (?, ?, ?, ?, ?, ?) \
                 ON CONFLICT (preview_id, service, revision) DO UPDATE SET at = excluded.at",
                params![id, service, revision, host, url, now],
            )?;
            let old: Vec<String> = c.query(
                "SELECT revision FROM pinned WHERE preview_id = ? AND service = ? ORDER BY at DESC LIMIT 1000 OFFSET ?",
                params![id, service, PINNED_KEEP as i64],
                |r| r.get(0),
            )?;
            for rev in &old {
                // Its host says it's gone (and where the current one is), not that nothing's there.
                c.execute(
                    "INSERT INTO gone_pushes (host, preview_id, service, revision, at) SELECT host, preview_id, service, revision, ? FROM pinned WHERE preview_id = ? AND service = ? AND revision = ? \
                     ON CONFLICT (host) DO UPDATE SET preview_id = excluded.preview_id, service = excluded.service, revision = excluded.revision, at = excluded.at",
                    params![now, id, service, rev],
                )?;
                c.execute("DELETE FROM pinned WHERE preview_id = ? AND service = ? AND revision = ?", params![id, service, rev])?;
            }
            // The latest few a preview's gone pushes say so; older ones are just gone.
            c.execute(
                "DELETE FROM gone_pushes WHERE preview_id = ? AND host NOT IN (SELECT host FROM gone_pushes WHERE preview_id = ? ORDER BY at DESC LIMIT ?)",
                params![id, id, GONE_KEPT as i64],
            )?;
            Ok::<_, Error>(old.into_iter().map(|rev| format!("{service}@{rev}")).collect())
        })
    }

    // ── note crops: a picture of each note's element, cropped by a screenshot run ──

    /// Keep a note's crop (a PNG of at most [`CROP_MAX`] bytes; a bigger one is dropped), at
    /// most [`CROPS_KEPT`] a preview (the oldest go).
    pub fn set_note_crop(&self, id: &str, note: &str, png: &[u8]) -> Result<()> {
        if png.len() > CROP_MAX || !png.starts_with(b"\x89PNG") {
            return Ok(());
        }
        use base64::Engine as _;
        let now = self.now_ms()?;
        let b64 = base64::engine::general_purpose::STANDARD.encode(png);
        self.db.write(|c| {
            c.execute(
                "INSERT INTO note_crops (preview_id, note_id, png, at) VALUES (?, ?, ?, ?) ON CONFLICT (preview_id, note_id) DO UPDATE SET png = excluded.png, at = excluded.at",
                params![id, note, b64, now],
            )?;
            c.execute(
                "DELETE FROM note_crops WHERE preview_id = ? AND note_id NOT IN (SELECT note_id FROM note_crops WHERE preview_id = ? ORDER BY at DESC, note_id LIMIT ?)",
                params![id, id, CROPS_KEPT as i64],
            )?;
            Ok::<_, Error>(())
        })
    }

    /// The notes of `id` that have a crop.
    pub fn cropped_notes(&self, id: &str) -> Result<std::collections::HashSet<String>> {
        Ok(self.db.read(|c| c.query("SELECT note_id FROM note_crops WHERE preview_id = ?", [id], |r| r.get(0)))?.into_iter().collect())
    }

    pub fn note_crop(&self, id: &str, note: &str) -> Result<Option<Vec<u8>>> {
        use base64::Engine as _;
        let row: Option<String> = self.db.read(|c| c.query_row("SELECT png FROM note_crops WHERE preview_id = ? AND note_id = ?", [id, note], |r| r.get(0)).optional())?;
        Ok(row.and_then(|b64| base64::engine::general_purpose::STANDARD.decode(b64).ok()))
    }

    // ── git_pulls: the pull request a Git URL service's preview is of (roll apps' `GitPull`) ──

    /// The pull request a preview is of: its number, link and whether it's from a fork, as roll
    /// apps last said. A new head of a fork clears the approval (it was for one head).
    pub fn set_git_pull(&self, id: &str, number: i64, url: &str, fork: bool, head: &str) -> Result<()> {
        self.db.write(|c| {
            c.execute(
                "INSERT INTO git_pulls (preview_id, number, url, fork) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (preview_id) DO UPDATE SET number = excluded.number, url = excluded.url, fork = excluded.fork",
                params![id, number, url, fork],
            )?;
            c.execute("UPDATE git_pulls SET approved_head = NULL, approved_by = NULL WHERE preview_id = ?1 AND approved_head IS NOT ?2", params![id, head])?;
            Ok::<_, Error>(())
        })
    }

    pub fn git_pull(&self, id: &str) -> Result<Option<crate::GitPullView>> {
        Ok(self.db.read(|c| {
            c.query_row("SELECT number, url, fork, approved_head, approved_by FROM git_pulls WHERE preview_id = ?", [id], |r| {
                Ok(crate::GitPullView { number: r.get(0)?, url: r.get(1)?, fork: r.get(2)?, approved_head: r.get(3)?, approved_by: r.get(4)? })
            })
            .optional()
        })?)
    }

    /// A writer let this head of a fork's pull request run. Only the head the preview is at:
    /// anything else is a conflict (it moved since the person looked).
    pub fn approve_git_pull(&self, id: &str, head: &str, by: &str) -> Result<()> {
        self.db.write(|c| {
            let at: Option<String> = c.query_row("SELECT head FROM previews WHERE id = ?", [id], |r| r.get(0)).optional()?;
            match at {
                None => return Err(Error::NotFound(format!("no preview {id}"))),
                Some(h) if h != head => return Err(Error::Conflict(format!("the pull request moved on to {}: approve that head", &h[..12.min(h.len())]))),
                Some(_) => {}
            }
            let n = c.execute("UPDATE git_pulls SET approved_head = ?2, approved_by = ?3 WHERE preview_id = ?1 AND fork = 1", params![id, head, by])?;
            if n == 0 {
                return Err(Error::Conflict("only a pull request from a fork waits for an approval".into()));
            }
            Ok(())
        })
    }

    /// What was last posted on a Git URL service's repository for a preview: the status (its sha,
    /// state and description, as one string), the pull request comment's id and its body's hash.
    /// What its holder last read of a preview's running code against main's preview (`kind`:
    /// `slower_routes`), kept so any node shows it.
    pub fn runtime_fact<T: serde::de::DeserializeOwned>(&self, id: &str, kind: &str) -> Result<Option<T>> {
        let json: Option<String> = self.db.read(|c| c.query_row("SELECT json FROM runtime_facts WHERE preview_id = ? AND kind = ?", [id, kind], |r| r.get(0)).optional())?;
        Ok(json.and_then(|j| serde_json::from_str(&j).ok()))
    }

    /// Keep what its holder read (`None`: nothing to say); whether that changed what was kept.
    /// A step of the timeline of `head` (`start` begins it): once each; a new head starts a new one.
    pub fn mark_step(&self, id: &str, head: &str, step: &str) -> Result<()> {
        let now = self.now_ms()?;
        self.db.write(|c| {
            let json: Option<String> = c.query_row("SELECT json FROM runtime_facts WHERE preview_id = ? AND kind = 'timeline'", [id], |r| r.get(0)).optional()?;
            let was: Option<crate::Timeline> = json.and_then(|j| serde_json::from_str(&j).ok()).filter(|t: &crate::Timeline| t.head == head);
            let mut t = match was {
                Some(t) if step == "start" || t.steps.iter().any(|x| x.step == step) => return Ok::<_, Error>(()),
                Some(t) => t,
                None => crate::Timeline { head: head.to_string(), started_at: now, steps: Vec::new() },
            };
            if step != "start" {
                t.steps.push(crate::TimelineStep { step: step.to_string(), at: now });
            }
            let json = serde_json::to_string(&t).map_err(|e| Error::Internal(e.to_string()))?;
            c.execute(
                "INSERT INTO runtime_facts (preview_id, kind, json) VALUES (?, 'timeline', ?) ON CONFLICT (preview_id, kind) DO UPDATE SET json = excluded.json",
                params![id, json],
            )?;
            Ok(())
        })
    }

    /// Change a preview's interception record in one write (`f` may run again).
    pub fn update_intercepted(&self, id: &str, f: impl Fn(&mut crate::Intercepted)) -> Result<()> {
        self.db.write(|c| {
            let json: Option<String> = c.query_row("SELECT json FROM runtime_facts WHERE preview_id = ? AND kind = 'intercepted'", [id], |r| r.get(0)).optional()?;
            let mut x: crate::Intercepted = json.and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
            f(&mut x);
            let json = serde_json::to_string(&x).map_err(|e| Error::Internal(e.to_string()))?;
            c.execute(
                "INSERT INTO runtime_facts (preview_id, kind, json) VALUES (?, 'intercepted', ?) ON CONFLICT (preview_id, kind) DO UPDATE SET json = excluded.json",
                params![id, json],
            )?;
            Ok::<_, Error>(())
        })
    }

    pub fn set_runtime_fact<T: serde::Serialize>(&self, id: &str, kind: &str, fact: Option<&T>) -> Result<bool> {
        let json = fact.map(serde_json::to_string).transpose().map_err(|e| Error::Internal(e.to_string()))?;
        self.db.write(|c| {
            let was: Option<String> = c.query_row("SELECT json FROM runtime_facts WHERE preview_id = ? AND kind = ?", [id, kind], |r| r.get(0)).optional()?;
            if was == json {
                return Ok::<_, Error>(false);
            }
            match &json {
                Some(j) => c.execute("INSERT INTO runtime_facts (preview_id, kind, json) VALUES (?1, ?2, ?3) ON CONFLICT (preview_id, kind) DO UPDATE SET json = excluded.json", params![id, kind, j])?,
                None => c.execute("DELETE FROM runtime_facts WHERE preview_id = ? AND kind = ?", [id, kind])?,
            };
            Ok(true)
        })
    }

    pub fn git_report(&self, id: &str) -> Result<(Option<String>, Option<i64>, Option<String>)> {
        Ok(self.db.read(|c| c.query_row("SELECT status, comment_id, comment_hash FROM git_reports WHERE preview_id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional())?.unwrap_or((None, None, None)))
    }

    pub fn set_git_report(&self, id: &str, status: Option<&str>, comment: Option<(i64, &str)>) -> Result<()> {
        self.db.write(|c| {
            c.execute("INSERT INTO git_reports (preview_id) VALUES (?1) ON CONFLICT (preview_id) DO NOTHING", [id])?;
            if let Some(s) = status {
                c.execute("UPDATE git_reports SET status = ?2 WHERE preview_id = ?1", params![id, s])?;
            }
            if let Some((cid, hash)) = comment {
                c.execute("UPDATE git_reports SET comment_id = ?2, comment_hash = ?3 WHERE preview_id = ?1", params![id, cid, hash])?;
            }
            Ok::<_, Error>(())
        })
    }

    /// The comment a Git URL service's previews keep on pull request `number` of `repo`, whichever
    /// project's preview posted it: the next project to speak for the service edits the same one.
    pub fn pull_comment(&self, service: &str, repo: &str, number: i64) -> Result<Option<i64>> {
        Ok(self.db.read(|c| c.query_row("SELECT comment_id FROM pull_comments WHERE service = ? AND repo = ? AND number = ?", params![service, repo, number], |r| r.get(0)).optional())?)
    }

    pub fn set_pull_comment(&self, service: &str, repo: &str, number: i64, comment_id: i64) -> Result<()> {
        self.db.write(|c| {
            c.execute("INSERT INTO pull_comments (service, repo, number, comment_id) VALUES (?1, ?2, ?3, ?4) ON CONFLICT (service, repo, number) DO UPDATE SET comment_id = excluded.comment_id", params![service, repo, number, comment_id])?;
            Ok::<_, Error>(())
        })
    }

    // ── leases: the dynamic credentials a service started with (roll secrets'; ids only) ──

    /// Lease ids a start of `service` was given, each thought to end `expires_at` (ms) until a
    /// renewal says otherwise.
    pub fn add_leases(&self, id: &str, service: &str, leases: &[String], expires_at: i64) -> Result<()> {
        let now = self.now_ms()?;
        self.db.write(|c| {
            for l in leases {
                c.execute(
                    "INSERT INTO leases (preview_id, service, lease, issued_at, expires_at) VALUES (?, ?, ?, ?, ?) ON CONFLICT (preview_id, lease) DO NOTHING",
                    params![id, service, l, now, expires_at],
                )?;
            }
            Ok::<_, Error>(())
        })
    }

    /// Every lease held, `id`'s only when given.
    pub fn leases(&self, id: Option<&str>) -> Result<Vec<crate::LeaseRow>> {
        let row = |r: &code_sql::Row<'_>| Ok(crate::LeaseRow { preview: r.get(0)?, service: r.get(1)?, lease: r.get(2)?, issued_at: r.get(3)?, expires_at: r.get(4)? });
        Ok(self.db.read(|c| match id {
            Some(id) => c.query("SELECT preview_id, service, lease, issued_at, expires_at FROM leases WHERE preview_id = ? ORDER BY issued_at", [id], row),
            None => c.query("SELECT preview_id, service, lease, issued_at, expires_at FROM leases ORDER BY issued_at", (), row),
        })?)
    }

    pub fn set_lease_expiry(&self, id: &str, lease: &str, expires_at: i64) -> Result<()> {
        self.db.write(|c| c.execute("UPDATE leases SET expires_at = ? WHERE preview_id = ? AND lease = ?", params![expires_at, id, lease]).map(drop))?;
        Ok(())
    }

    pub fn drop_lease(&self, id: &str, lease: &str) -> Result<()> {
        self.db.write(|c| c.execute("DELETE FROM leases WHERE preview_id = ? AND lease = ?", params![id, lease]).map(drop))?;
        Ok(())
    }

    /// A link that opens the preview until `expires_at` (Vercel's shareable links), on a token of
    /// its own: only its SHA-256 is kept, so the token is in the answer once. At most
    /// [`SHARES_MAX`] live ones.
    pub fn create_share(&self, id: &str, expires_at: i64, by: &str, maker: Option<&str>) -> Result<(crate::Share, String)> {
        let now = self.now_ms()?;
        let token = format!("rps_{}", new_token());
        let sid = format!("shr_{}", &new_token()[..12]);
        let hash = bypass_hash(&token);
        self.db.write(|c| {
            let live: i64 = c.query_row("SELECT COUNT(*) FROM shares WHERE preview_id = ? AND revoked = 0 AND expires_at > ?", params![id, now], |r| r.get(0))?;
            if live as usize >= SHARES_MAX {
                return Err(Error::Conflict(format!("the preview has {SHARES_MAX} live share links: revoke one first")));
            }
            c.execute("INSERT INTO shares (id, preview_id, hash, expires_at, created_by, created_at, maker, maker_checked) VALUES (?, ?, ?, ?, ?, ?, ?, ?)", params![sid, id, hash, expires_at, by, now, maker, now])?;
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

    /// A live share link `token` of preview `id` (not revoked, not past).
    pub fn share_live(&self, id: &str, token: &str) -> Result<Option<LiveShare>> {
        let now = self.now_ms()?;
        Ok(self.db.read(|c| {
            c.query_row(
                "SELECT id, expires_at, maker, maker_checked FROM shares WHERE preview_id = ? AND hash = ? AND revoked = 0 AND expires_at > ?",
                params![id, bypass_hash(token), now],
                |r| Ok(LiveShare { id: r.get(0)?, ends: r.get(1)?, maker: r.get(2)?, maker_checked: r.get(3)? }),
            )
            .optional()
        })?)
    }

    /// A share link's maker was just asked about (and still may).
    pub fn share_maker_checked(&self, share: &str, now: i64) -> Result<()> {
        self.db.write(|c| c.execute("UPDATE shares SET maker_checked = ? WHERE id = ?", params![now, share]).map(drop))?;
        Ok(())
    }

    /// When the share link `token` of preview `id` ends, if it's live (not revoked, not past).
    pub fn share_ends(&self, id: &str, token: &str) -> Result<Option<i64>> {
        let now = self.now_ms()?;
        Ok(self.db.read(|c| {
            c.query_row("SELECT expires_at FROM shares WHERE preview_id = ? AND hash = ? AND revoked = 0 AND expires_at > ?", params![id, bypass_hash(token), now], |r| r.get(0)).optional()
        })?)
    }

    // ── signed-in visitors (`access: team`, `visit.rs`) ──

    /// A one-use ticket for `user` (`handle`, signed in as roll session `roll_session`) into
    /// preview `id`, kept by its SHA-256. `false`, and nothing kept, unless the project has team
    /// access: read in the same write, so a change of access (which deletes tickets) can't miss it.
    #[allow(clippy::too_many_arguments)]
    pub fn put_visit_ticket(&self, hash: &str, id: &str, project: &str, user: &str, handle: &str, roll_session: &str, expires_at: i64) -> Result<bool> {
        self.db.write(|c| {
            if !team_access(c, project)? {
                return Ok(false);
            }
            c.execute(
                "INSERT INTO visit_tickets (hash, preview_id, project_id, user_id, handle, roll_session, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
                params![hash, id, project, user, handle, roll_session, expires_at],
            )?;
            Ok::<_, Error>(true)
        })
    }

    /// A ticket used: it goes either way (and every expired one with it), and if it was this
    /// preview's, still good, and its project still has team access, a session (`session`, by
    /// its SHA-256, until `expires_at`) takes its place in the same write. The session's
    /// person, roll session and project. Who it is goes in the preview's activity (once per
    /// session: a session is opened once) and its visitors ([`Store::visitors`]).
    pub fn open_visit_session(&self, ticket: &str, session: &str, id: &str, expires_at: i64, now: i64) -> Result<Option<VisitSession>> {
        self.db.write(|c| {
            let row: Option<(String, String, String, String, i64, String)> = c
                .query_row("SELECT preview_id, project_id, user_id, roll_session, expires_at, handle FROM visit_tickets WHERE hash = ?", [ticket], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
                })
                .optional()?;
            c.execute("DELETE FROM visit_tickets WHERE hash = ? OR expires_at < ?", params![ticket, now])?;
            let Some((_, project, user, roll_session, _, handle)) = row.filter(|(p, _, _, _, exp, _)| p == id && *exp >= now) else { return Ok(None) };
            if !team_access(c, &project)? {
                return Ok(None);
            }
            c.execute("DELETE FROM visit_sessions WHERE expires_at < ?", [now])?;
            c.execute(
                "INSERT INTO visit_sessions (hash, preview_id, project_id, user_id, roll_session, expires_at, checked_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
                params![session, id, project, user, roll_session, expires_at, now],
            )?;
            // A ticket from a node that predates handles has none: the handle kept stays.
            let who = if handle.is_empty() { user.as_str() } else { handle.as_str() };
            let last: Option<i64> = c.query_row("SELECT last_at FROM visits WHERE preview_id = ? AND user_id = ?", [id, user.as_str()], |r| r.get(0)).optional()?;
            c.execute(
                "INSERT INTO visits (preview_id, user_id, handle, first_at, last_at) VALUES (?, ?, ?, ?, ?) ON CONFLICT (preview_id, user_id) DO UPDATE SET handle = CASE WHEN ? = '' THEN visits.handle ELSE excluded.handle END, last_at = excluded.last_at",
                params![id, user, who, now, now, handle],
            )?;
            // In the activity once a day per person: a new device or an ended session isn't news,
            // and the log keeps the last ACTIVITY_KEEP lines.
            if last.is_none_or(|t| now - t >= 86_400_000) {
                append(c, id, now, &format!("{who} opened it (signed in to roll)"))?;
            }
            Ok::<_, Error>(Some(VisitSession { user, roll_session, project, expires_at, checked_at: now }))
        })
    }

    /// A session in preview `id`: its person, project, when it ends and when its person was last
    /// asked about.
    pub fn visit_session(&self, hash: &str, id: &str) -> Result<Option<VisitSession>> {
        Ok(self.db.read(|c| {
            c.query_row("SELECT user_id, roll_session, project_id, expires_at, checked_at FROM visit_sessions WHERE hash = ? AND preview_id = ?", [hash, id], |r| {
                Ok(VisitSession { user: r.get(0)?, roll_session: r.get(1)?, project: r.get(2)?, expires_at: r.get(3)?, checked_at: r.get(4)? })
            })
            .optional()
        })?)
    }

    /// Its person (`user`, in preview `id`) may still open the project, as of `now`: also when
    /// they were last in it ([`Store::visitors`]), so that's as fresh as the recheck.
    pub fn visit_checked(&self, hash: &str, id: &str, user: &str, now: i64) -> Result<()> {
        self.db.write(|c| {
            c.execute("UPDATE visit_sessions SET checked_at = ? WHERE hash = ?", params![now, hash])?;
            c.execute("UPDATE visits SET last_at = ? WHERE preview_id = ? AND user_id = ? AND last_at < ?", params![now, id, user, now])?;
            Ok::<_, Error>(())
        })
    }

    /// Who signed in to preview `id` (`access: team`), the last [`VISITORS`] by when they were
    /// last in it (as of a session's recheck), latest first.
    pub fn visitors(&self, id: &str) -> Result<Vec<crate::Visitor>> {
        Ok(self.db.read(|c| {
            c.query(&format!("SELECT handle, last_at FROM visits WHERE preview_id = ? ORDER BY last_at DESC, handle LIMIT {VISITORS}"), [id], |r| Ok(crate::Visitor { who: r.get(0)?, last_at: r.get(1)? }))
        })?)
    }

    /// Share link `token` of preview `id` was followed on `day` (days since 1970): a line in the
    /// preview's activity, once per link per day. Whether this was the day's first.
    /// A read first: a client that keeps no cookie follows the link on every request, and only the
    /// day's first is a write.
    pub fn share_opened(&self, id: &str, token: &str, day: i64, now: i64) -> Result<bool> {
        let hash = bypass_hash(token);
        let seen: Option<Option<i64>> = self.db.read(|c| c.query_row("SELECT opened_day FROM shares WHERE preview_id = ? AND hash = ?", params![id, hash], |r| r.get(0)).optional())?;
        if seen.flatten().is_some_and(|d| d >= day) {
            return Ok(false);
        }
        self.db.write(|c| {
            let first = c.execute("UPDATE shares SET opened_day = ? WHERE preview_id = ? AND hash = ? AND (opened_day IS NULL OR opened_day < ?)", params![day, id, bypass_hash(token), day])? > 0;
            if first {
                append(c, id, now, "a share link opened it")?;
            }
            Ok::<_, Error>(first)
        })
    }

    pub fn end_visit_session(&self, hash: &str) -> Result<()> {
        self.db.write(|c| {
            c.execute("DELETE FROM visit_sessions WHERE hash = ?", [hash])?;
            Ok::<_, Error>(())
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

    /// What a preview costs (asleep a month, active an hour, this month so far), without the rest
    /// of its view: what the budget adds up each pass.
    pub fn cost(&self, r: &Row) -> Result<Cost> {
        self.cost_of(r, &self.resources(&r.id)?, &self.services_with_time(&r.id)?)
    }

    fn cost_of(&self, r: &Row, resources: &[ResourceRow], services: &[(Service, i64)]) -> Result<Cost> {
        let mut cost = Cost::default();
        let now = self.now_ms()?;
        let month_start = month_start(now);
        let held_hours = (now - r.created_at.max(month_start)).max(0) as f64 / HOUR_MS;
        for res in resources {
            cost.bytes += res.bytes;
            cost.sleeping_usd_month += res.usd_month;
            cost.active_usd_hour += res.usd_hour;
            // Storage since the start of the month at today's size, plus what the product
            // metered for compute.
            cost.month_to_date_usd += res.usd_month * held_hours / MONTH_HOURS + res.spent_usd;
        }
        // Earlier pushes' copies of the data: kept like the branches (storage only; their compute
        // runs as the preview's does, when someone opens one).
        for (bytes, usd_month, spent) in self.pinned_usage(&r.id)? {
            cost.bytes += bytes;
            cost.sleeping_usd_month += usd_month;
            cost.month_to_date_usd += usd_month * held_hours / MONTH_HOURS + spent;
        }
        for (s, active_ms) in services {
            let gib = s.memory_mb as f64 / 1024.0;
            cost.active_usd_hour += gib * self.rates.usd_per_container_gib_hour;
            cost.month_to_date_usd += gib * self.rates.usd_per_container_gib_hour * (*active_ms as f64 / HOUR_MS);
        }
        Ok(cost)
    }

    pub fn view(&self, r: Row) -> Result<Preview> {
        let settings = self.settings(&r.project_id)?;
        let resources = self.resources(&r.id)?;
        let services = self.services_with_time(&r.id)?;
        let cost = self.cost_of(&r, &resources, &services)?;
        // With link access, the links roll hands out carry the token (once; then a cookie). With
        // team access they're plain: people sign in instead.
        let token = self.access(&r.id)?.and_then(|a| a.link_token().map(str::to_string));
        let share = |u: &str| match &token {
            Some(t) => format!("{}/?roll_access={t}", u.trim_end_matches('/')),
            None => u.to_string(),
        };
        let url = services.first().map(|(s, _)| s.url.clone());
        let share_url = url.as_deref().map(share);
        let (schema_changes, schema_risks, release) = self.release_facts(&r.id)?;
        let kept_until = self.kept_until(&r.id)?;
        // Its page as a link only when it's `https://` (it's kept whole to tell its repository).
        let pull = self.git_pull(&r.id)?.map(|mut g| {
            if !g.url.starts_with("https://") {
                g.url.clear();
            }
            g
        });
        let goes_at = self.goes_at(&r.id)?;
        let reported_by = self.reported_by(&r.id)?;
        // The screenshot row in one read, without its images.
        let f = self.shot_facts(&r.id)?;
        let screenshot_at = f.at;
        let phone_percent = f.phone_change_bp.map(|bp| bp as f64 / 100.0);
        let visual_change = f.prev_head.clone().zip(f.change_bp).map(|(since, bp)| crate::VisualChange { since, percent: bp as f64 / 100.0, phone_percent });
        let phone_screenshot = f.has_phone;
        let pages = self.page_checks(&r.id)?;
        let baseline = r.is_baseline();
        let (page_a11y, page_health, page_links) = (f.a11y, f.health, f.links);
        let base_row = f.base;
        let base_differs = base_row.as_ref().is_some_and(|b| b.change_bp > 0);
        let base_change = base_row.map(|b| crate::BaseChange {
            branch: b.branch,
            percent: b.change_bp as f64 / 100.0,
            phone_percent: b.phone_bp.map(|p| p as f64 / 100.0),
            head: b.head,
            load_ms: b.speed.as_ref().map(|s| s.load_ms),
            lcp_ms: b.speed.as_ref().and_then(|s| s.lcp_ms),
            a11y_added: page_a11y.as_ref().zip(b.a11y.as_ref()).map(|(ours, main)| ours.added_since(main)),
            errors_added: page_health.as_ref().zip(b.health.as_ref()).map(|(ours, main)| ours.added_since(main)),
            text: b.text,
            seo_added: page_a11y.as_ref().and_then(|a| a.seo.as_ref()).zip(b.a11y.as_ref().and_then(|a| a.seo.as_ref())).map(|(ours, main)| ours.added_since(main)),
            links_added: page_links.as_ref().zip(b.links.as_ref()).map(|(ours, main)| ours.added_since(main)),
            api: b.api,
            api_breaking: b.api_breaking,
            api_unread: b.api_unread,
            endpoints: f.speed.as_ref().map(|now| {
                let main = b.speed.as_ref().map(|s| s.endpoints.as_slice()).unwrap_or_default();
                now.endpoints.iter().map(|e| crate::EndpointAgainst { path: e.path.clone(), ms: e.ms, status: e.status, main_ms: main.iter().find(|m| m.path == e.path).map(|m| m.ms) }).collect()
            }).unwrap_or_default(),
        });
        let first_unstable = page_health.as_ref().is_some_and(|h| h.unstable);
        let visual_review = self.visual_review(&r, settings.visual_review, settings.github.visual_threshold, base_change.as_ref(), &pages, first_unstable, f.head)?;
        let page_speed = f.speed.map(|now| crate::PageSpeedView { now, before: f.prev_speed });
        // What the holder read against main's, for this head only (a push reads it again), and
        // whether it compared at this head at all (errors; routes too with traces).
        let new_errors = self.runtime_fact::<crate::NewErrors>(&r.id, "new_errors")?.filter(|f| f.head == r.head);
        let slower_routes = if settings.traces { self.runtime_fact::<crate::SlowerRoutes>(&r.id, "slower_routes")?.filter(|f| f.head == r.head) } else { None };
        let compared_at = |kind: &str| -> Result<bool> { Ok(self.runtime_fact::<String>(&r.id, kind)?.as_deref() == Some(r.head.as_str())) };
        let regressions_compared = compared_at("compared_errors")? && (!settings.traces || compared_at("compared_routes")?);
        // Its own proxy's record (with the keys it holds), and roll apps' counts for its services there.
        let closed = settings.egress_for(r.trusted) == crate::settings::EgressRule::Roll;
        let on_apps = self.runtime_fact::<crate::AppsIntercepted>(&r.id, "intercepted_apps")?.filter(|_| closed);
        let timeline = self.runtime_fact::<crate::Timeline>(&r.id, "timeline")?.filter(|t| t.head == r.head);
        let intercepted = match (self.runtime_fact::<crate::Intercepted>(&r.id, "intercepted")?.filter(|x| !x.keys.is_empty()), on_apps) {
            (Some(x), on_apps) => Some(crate::Intercepted { on_apps, ..x }),
            (None, Some(a)) => Some(crate::Intercepted { on_apps: Some(a), ..Default::default() }),
            (None, None) => None,
        };
        let regressions = match settings.regressions {
            crate::settings::RegressionsRule::Off => None,
            crate::settings::RegressionsRule::Check => Some("check".to_string()),
            crate::settings::RegressionsRule::Required => Some("required".to_string()),
        };
        // An acceptance counts at its head, for what it accepted (more at that head asks again), and
        // for a required rule only a person's.
        let showing = crate::RegressionsAccepted::of(slower_routes.as_ref(), new_errors.as_ref());
        let errors_total = new_errors.as_ref().map_or(0, |n| n.total);
        let regressions_accepted = self
            .runtime_fact::<crate::RegressionsAccepted>(&r.id, "regressions_accepted")?
            .filter(|a| a.head == r.head && errors_total <= a.errors_total && showing.iter().all(|w| a.what.contains(w)))
            .filter(|a| a.person || settings.regressions != crate::settings::RegressionsRule::Required);
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
            reaches: settings.egress_for(r.trusted),
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
                .map(|x| {
                    let masks = settings.masking.get(&format!("{}:{}", x.kind.as_str(), x.parent)).map_or(0, |m| m.len());
                    Resource { data: crate::Data::of(x.kind, x.mode, masks), kind: x.kind, parent: x.parent, r#ref: x.r#ref, mode: x.mode, state: x.state, error: x.error, bytes: x.bytes }
                })
                .collect(),
            cost: cost.rounded(),
            schema_risks,
            schema_changes,
            release,
            kept_until,
            pull,
            goes_at,
            reported_by,
            earlier,
            screenshot_at,
            visual_change,
            phone_screenshot,
            base_change,
            pages,
            visual_review,
            page_links,
            slower_routes,
            new_errors,
            regressions,
            regressions_accepted,
            regressions_compared,
            intercepted,
            timeline,
            page_speed,
            page_health,
            page_a11y,
            screenshot_link,
            diff_link,
            notes: Vec::new(),
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

/// Whether `project`'s previews let people in by signing in (`access: team`), inside a write.
fn team_access(c: &code_sql::Conn<'_>, project: &str) -> Result<bool> {
    let json: Option<String> = c.query_row("SELECT json FROM settings WHERE project_id = ?", [project], |r| r.get(0)).optional()?;
    Ok(json.and_then(|j| serde_json::from_str::<Settings>(&j).ok()).is_some_and(|s| s.access == crate::settings::Access::Team))
}

/// A preview's access token: 24 random characters of lowercase base32 (120 bits).
pub(crate) fn new_token() -> String {
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
    /// The first page's words against its (`visual::text_diff`).
    pub text: Option<crate::TextDiff>,
    /// Its first page's broken links there.
    pub links: Option<crate::PageLinks>,
    /// Its API's operations against the change's (`visual::text_diff`).
    pub api: Option<crate::TextDiff>,
    /// What the change breaks inside the operations both keep.
    pub api_breaking: Vec<String>,
    /// Main's API document was read and the change's wasn't.
    pub api_unread: bool,
}

/// Live share links a preview may have at once.
pub const SHARES_MAX: usize = 20;

/// Earlier pushes kept reachable per service.
pub const PINNED_KEEP: usize = 3;

/// An earlier push its preview no longer keeps ([`Store::gone_push`]): whose it was, and the
/// service's current URL (none once the preview no longer runs it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GonePush {
    pub preview: String,
    pub service: String,
    pub revision: String,
    pub current_url: Option<String>,
}
/// How many of a preview's evicted pushes still say they're gone.
pub const GONE_KEPT: usize = 30;

/// How long an event is kept for followers (`GET /v1/previews/events`).
const EVENTS_KEEP_MS: i64 = 7 * 24 * 3_600_000;

/// Record an event in the write that made it so. Its `seq` is taken inside that write (never a
/// sequence, whose values are handed out outside it), so on Postgres two writers conflict and one
/// re-runs: commit order is seq order, and a follower past one never misses an earlier one.
fn emit(c: &Conn<'_>, at: i64, kind: &str, project: &str, preview: &str, change: Option<&str>, data: &serde_json::Value) -> code_sql::Result<()> {
    let seq: i64 = c.query_row("SELECT COALESCE(MAX(seq), 0) + 1 FROM events", (), |r| r.get(0))?;
    c.execute(
        "INSERT INTO events (seq, at, kind, project_id, preview_id, change_id, data) VALUES (?, ?, ?, ?, ?, ?, ?)",
        params![seq, at, kind, project, preview, change, data.to_string()],
    )?;
    c.execute("DELETE FROM events WHERE at < ?", [at - EVENTS_KEEP_MS])?;
    Ok(())
}

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
        let speed = |load_ms| crate::PageSpeed { ttfb_ms: 20, fcp_ms: Some(100), load_ms, bytes: 1000, lcp_ms: None, cls: None, endpoints: Vec::new() };
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
        assert_eq!(store.base_change(id).unwrap(), Some(BaseRow { preview: main.id.clone(), branch: "main".into(), change_bp: 2000, phone_bp: Some(5000), head: Some("cs_m".into()), speed: None, a11y: None, health: None, text: None, links: None, api: None, api_breaking: Vec::new(), api_unread: false }));
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
        let (share, token) = store.create_share(id, now + 3_600_000, "ada", None).unwrap();
        assert!(token.starts_with("rps_") && share.id.starts_with("shr_"));
        assert_eq!(store.share_ends(id, &token).unwrap(), Some(now + 3_600_000));
        assert_eq!(store.share_ends("pv_other", &token).unwrap(), None, "another preview");
        assert_eq!(store.share_ends(id, "rps_guess").unwrap(), None);
        let (_, ended) = store.create_share(id, now - 1, "ada", None).unwrap();
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

    /// What previews keeps that's a credential is sealed at rest with the deployment key, bound to
    /// its row: a preview's own variables, the rules' pasted values (a reference stays as it is)
    /// and the screenshot links' key. A database from before, in the clear, is sealed when the key
    /// comes, and nothing of it stays readable in the file or its `-wal`; with no key, a pasted
    /// value is refused, never written in the clear.
    #[test]
    fn credentials_are_sealed_at_rest_and_nothing_stays_in_the_file() {
        struct Hex;
        impl crate::ports::Sealer for Hex {
            fn seal(&self, aad: &str, plain: &str) -> String {
                format!("v1:{}|{}", hex::encode(aad), hex::encode(plain))
            }
            fn open(&self, aad: &str, sealed: &str) -> Option<String> {
                String::from_utf8(hex::decode(sealed.strip_prefix(&format!("v1:{}|", hex::encode(aad)))?).ok()?).ok()
            }
        }
        let (dir, store) = store();
        let (a, _) = store.create(&new("chg_va")).unwrap();
        let (b, _) = store.create(&new("chg_vb")).unwrap();
        // No key: a pasted value is refused (a reference isn't one).
        let refused = store.update_vars(&a.id, |v| {
            v.insert("STRIPE_KEY".into(), "sk_test_51Hx9".into());
            Ok(())
        });
        assert!(matches!(refused, Err(Error::BadRequest(_))));
        let mut rules = Settings::default();
        rules.env.insert("TOKEN".into(), "tok_rules_77".into());
        assert!(matches!(store.put_settings("prj_1", &rules), Err(Error::BadRequest(_))));
        rules.env.insert("TOKEN".into(), "roll://staging/TOKEN".into());
        store.put_settings("prj_1", &rules).unwrap();
        // An upgraded database: what an older version wrote in the clear.
        let mut old = Settings::default();
        old.env.insert("TOKEN".into(), "tok_rules_77".into());
        store
            .db
            .write(|c| {
                c.execute("UPDATE settings SET json = ? WHERE project_id = 'prj_1'", [serde_json::to_string(&old).unwrap()])?;
                c.execute("UPDATE previews SET vars = ? WHERE id = ?", params![r#"{"STRIPE_KEY":"sk_test_51Hx9"}"#, a.id])?;
                c.execute("INSERT INTO keys (name, value) VALUES ('screenshots', ?) ON CONFLICT (name) DO UPDATE SET value = excluded.value", ["5c4e7f00aa11bb22"])
            })
            .unwrap();
        // Long since in the database file itself, not only its WAL.
        store.db.write_erasing(|_| Ok::<_, code_sql::Error>(())).unwrap();
        assert!(std::fs::read(dir.path().join("p.db")).unwrap().windows(13).any(|w| w == b"sk_test_51Hx9"), "the premise: it's in the file");
        store.set_sealer(std::sync::Arc::new(Hex)).unwrap();
        // Read back as written, through the key.
        assert_eq!(store.vars(&a.id).unwrap()["STRIPE_KEY"], "sk_test_51Hx9");
        assert_eq!(store.settings("prj_1").unwrap().env["TOKEN"], "tok_rules_77");
        // Nothing of it in the file, its free pages or its WAL.
        for f in ["p.db", "p.db-wal"] {
            let bytes = std::fs::read(dir.path().join(f)).unwrap_or_default();
            for v in ["sk_test_51Hx9", "tok_rules_77", "5c4e7f00aa11bb22"] {
                assert!(!bytes.windows(v.len()).any(|w| w == v.as_bytes()), "{v} in {f}");
            }
        }
        // Bound to its row: copied onto another preview, it doesn't open.
        let sealed: String = store.db.read(|c| c.query_row("SELECT vars FROM previews WHERE id = ?", [a.id.as_str()], |r| r.get(0))).unwrap();
        store.db.write(|c| c.execute("UPDATE previews SET vars = ? WHERE id = ?", params![sealed, b.id]).map(drop)).unwrap();
        assert!(store.vars(&b.id).is_err());
    }

    /// An earlier push's copy of the data costs what it holds, in the preview's cost (and its
    /// budget and size cap with it).
    #[test]
    fn an_earlier_pushs_copy_of_the_data_counts_in_the_previews_cost() {
        let (_dir, store) = store();
        let (row, _) = store.create(&new("chg_copy")).unwrap();
        let before = store.cost(&row).unwrap();
        let d = PinnedData { revision: "cs_1".into(), kind: Kind::Postgres, parent: "prj_db".into(), name: "pv-x-1".into(), r#ref: "br_1".into(), kept: None, source: "br_0".into() };
        store.add_pinned_data(&row.id, &d).unwrap();
        store.set_pinned_usage(&row.id, &d, &crate::ports::Usage { bytes: 3 << 20, storage_usd_month: 0.5, active_usd_hour: 0.0, month_to_date_usd: 0.25 }).unwrap();
        let after = store.cost(&row).unwrap();
        assert_eq!(after.bytes - before.bytes, 3 << 20);
        assert!((after.sleeping_usd_month - before.sleeping_usd_month - 0.5).abs() < 1e-9);
        assert!(after.month_to_date_usd - before.month_to_date_usd >= 0.25);
    }

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

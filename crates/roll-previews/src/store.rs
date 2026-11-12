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
CREATE TABLE IF NOT EXISTS activity (
    preview_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    at INTEGER NOT NULL,
    message TEXT NOT NULL,
    PRIMARY KEY (preview_id, seq)
) WITHOUT ROWID;
";

/// The newest activity lines kept per preview.
pub const ACTIVITY_KEEP: i64 = 200;
/// The longest slug: leaves room for a service name in a 63-character host label.
pub const SLUG_MAX: usize = 40;

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
}

pub struct Store {
    db: Db,
    rates: Rates,
}

const COLUMNS: &str = "id, project_id, slug, repo, change_id, branch, number, title, github_pr, head, trusted, desired, state, error, attempts, \
     next_at, asleep, reset_requested, last_reset_at, last_active_at, expires_at, created_by, created_at, updated_at";

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
    })
}

const RESOURCE_COLUMNS: &str = "kind, parent, mode, ref, state, error, bytes, usd_month, usd_hour, spent_usd";

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
        },
        r.get(9)?,
    ))
}

const SERVICE_COLUMNS: &str = "name, repo, host, url, revision, deployed, state, error, memory_mb, active_ms";

fn json_list(v: &Option<Vec<String>>) -> Option<String> {
    v.as_ref().map(|l| serde_json::to_string(l).expect("JSON"))
}

impl Store {
    pub fn open(target: &Target, pg: code_sql::PgOptions) -> Result<Store> {
        let db = match target {
            Target::Sqlite(path) => Db::open_sqlite(path, code_sql::SqliteOptions { readers: 2, cache_kib: 2 << 10 })?,
            Target::Postgres { url, schema } => Db::connect(url, schema, pg)?,
        };
        Store::new(db)
    }

    pub fn new(db: Db) -> Result<Store> {
        db.ensure_schema(SCHEMA)?;
        Ok(Store { db, rates: Rates::default() })
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
                &format!("INSERT INTO previews ({COLUMNS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'active', ?, NULL, 0, 0, 0, NULL, NULL, ?, ?, ?, ?, ?)"),
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
            c.execute(
                "UPDATE previews SET title = ?, head = ?, trusted = ?, github_pr = ?, desired = ?, state = ?, error = ?, attempts = ?, next_at = ?, \
                 asleep = ?, reset_requested = ?, last_reset_at = ?, last_active_at = ?, expires_at = ?, updated_at = ? WHERE id = ?",
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
            c.execute("UPDATE previews SET last_active_at = ? WHERE id = ? AND last_active_at < ?", params![now, id, now - 60_000])?;
            Ok::<_, Error>(())
        })
    }

    /// Remove a preview's rows (after its branches and services are gone).
    pub fn remove(&self, id: &str) -> Result<()> {
        self.db.write(|c| {
            for t in ["resources", "services", "activity", "previews"] {
                let col = if t == "previews" { "id" } else { "preview_id" };
                c.execute(&format!("DELETE FROM {t} WHERE {col} = ?"), [id])?;
            }
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
                    "INSERT INTO resources (preview_id, kind, parent, mode, ref, state, error, bytes, usd_month, usd_hour, spent_usd, updated_at) \
                     VALUES (?, ?, ?, ?, NULL, 'pending', NULL, 0, 0, 0, 0, ?) ON CONFLICT (preview_id, kind, parent) DO NOTHING",
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
                    "UPDATE resources SET mode = ?, ref = ?, state = ?, error = ?, bytes = ?, usd_month = ?, usd_hour = ?, spent_usd = ?, updated_at = ? \
                     WHERE preview_id = ? AND kind = ? AND parent = ?",
                    params![r.mode.as_str(), r.r#ref, r.state.as_str(), r.error, r.bytes, r.usd_month, r.usd_hour, r.spent_usd, now, id, kind.as_str(), parent],
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
    pub fn want_service(&self, id: &str, slug: &str, name: &str, repo: &str, revision: &str, memory_mb: u32, url: &dyn Fn(&str) -> String) -> Result<Service> {
        let now = self.now_ms()?;
        self.db.write(|c| {
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

    /// Add `ms` of running time to a service (its share of the month's cost).
    pub fn add_active(&self, id: &str, name: &str, ms: i64) -> Result<()> {
        self.db.write(|c| {
            c.execute("UPDATE services SET active_ms = active_ms + ? WHERE preview_id = ? AND name = ?", params![ms, id, name])?;
            Ok::<_, Error>(())
        })
    }

    pub fn drop_service(&self, id: &str, name: &str) -> Result<()> {
        self.db.write(|c| {
            c.execute("DELETE FROM services WHERE preview_id = ? AND name = ?", [id, name])?;
            Ok::<_, Error>(())
        })
    }

    /// The preview and service a host label serves.
    pub fn service_for_host(&self, host: &str) -> Result<Option<(String, String)>> {
        Ok(self.db.read(|c| c.query_row("SELECT preview_id, name FROM services WHERE host = ?", [host], |r| Ok((r.get(0)?, r.get(1)?))).optional())?)
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
        let url = services.first().map(|(s, _)| s.url.clone());
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
            head: r.head,
            trusted: r.trusted,
            desired: r.desired,
            state: r.state,
            error: r.error,
            url,
            services: services.into_iter().map(|(s, _)| s).collect(),
            resources: resources
                .into_iter()
                .map(|x| Resource { kind: x.kind, parent: x.parent, r#ref: x.r#ref, mode: x.mode, state: x.state, error: x.error, bytes: x.bytes })
                .collect(),
            cost: cost.rounded(),
            last_reset_at: r.last_reset_at,
            last_active_at: r.last_active_at,
            expires_at: r.expires_at,
            created_by: r.created_by,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
    }
}

fn slug_taken(c: &Conn<'_>, slug: &str) -> code_sql::Result<bool> {
    Ok(c.query_row("SELECT 1 FROM previews WHERE slug = ?", [slug], |_| Ok(())).optional()?.is_some())
}

fn host_taken(c: &Conn<'_>, host: &str) -> code_sql::Result<bool> {
    Ok(c.query_row("SELECT 1 FROM services WHERE host = ?", [host], |_| Ok(())).optional()?.is_some())
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
        let web = s.want_service(&a.id, &a.slug, "web", "acme/web", "cs_1", 512, &url).unwrap();
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

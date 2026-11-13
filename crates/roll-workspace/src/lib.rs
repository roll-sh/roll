//! Workspace projects (docs/workspace.md): product resources grouped into projects, each with its
//! place on the dashboard's canvas.
//!
//! The data is two tables in one `code_sql` database: SQLite at `<home>/workspace.db` on one
//! node, or the system database's `workspace` schema shared by every node. Every read-then-write
//! is one [`Db::write`] closure, so nodes need no leases. Membership only points at resources:
//! nothing here creates or deletes a product's resource.
//!
//! Every project has an owner: a signed-in user's id (docs/accounts.md), or empty for the
//! operator's. A [`Scope`] says which projects a caller sees; names and resource places are per
//! owner.
//!
//! [`api::router`] serves it under `/v1/workspace`.

pub mod api;

use code_sql::{Db, OptionalExtension, Target, params};
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::HashMap;

pub use api::router;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS projects (
    id TEXT PRIMARY KEY,
    owner TEXT NOT NULL DEFAULT '',
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    icon TEXT,
    created_at INTEGER NOT NULL,
    UNIQUE (owner, name)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS resources (
    owner TEXT NOT NULL DEFAULT '',
    kind TEXT NOT NULL,
    ref TEXT NOT NULL,
    project_id TEXT NOT NULL,
    x REAL NOT NULL,
    y REAL NOT NULL,
    added_at INTEGER NOT NULL,
    PRIMARY KEY (owner, kind, ref)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS resources_project ON resources (project_id);
";

/// Which projects a caller sees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope<'a> {
    /// Every project: the operator and admins.
    All,
    /// Only the projects of this owner (a user's id).
    Owner(&'a str),
}

impl Scope<'_> {
    fn sees(&self, owner: &str) -> bool {
        match self {
            Scope::All => true,
            Scope::Owner(o) => *o == owner,
        }
    }
}

/// The longest project name.
pub const NAME_MAX: usize = 48;
/// The longest resource ref, in characters.
pub const REF_MAX: usize = 200;
/// The longest description, in characters.
pub const DESCRIPTION_MAX: usize = 280;
/// The biggest picture an icon may carry, as its `data:` URL.
pub const IMAGE_MAX: usize = 200 * 1024;
/// The colours an icon may have (`--id-<name>` in the dashboard's globals.css).
pub const COLORS: [&str; 8] = ["slate", "violet", "purple", "pink", "rose", "teal", "cyan", "green"];

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("workspace database: {0}")]
    Db(#[from] code_sql::Error),
    #[error("{0}")]
    Internal(String),
}

/// What a resource is: the product it belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Postgres,
    Redis,
    Stream,
    Bucket,
    Nosql,
    Search,
    Repo,
}

impl Kind {
    pub const ALL: [Kind; 7] = [Kind::Postgres, Kind::Redis, Kind::Stream, Kind::Bucket, Kind::Nosql, Kind::Search, Kind::Repo];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Postgres => "postgres",
            Kind::Redis => "redis",
            Kind::Stream => "stream",
            Kind::Bucket => "bucket",
            Kind::Nosql => "nosql",
            Kind::Search => "search",
            Kind::Repo => "repo",
        }
    }

    pub fn parse(s: &str) -> Result<Kind> {
        Kind::ALL.into_iter().find(|k| k.as_str() == s).ok_or_else(|| {
            let all = Kind::ALL.map(Kind::as_str).join(", ");
            Error::BadRequest(format!("unknown resource kind {s:?}: one of {all}"))
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Project {
    pub id: String,
    /// The user it belongs to (`usr_…`), or empty for the operator's.
    pub owner: String,
    pub name: String,
    pub description: String,
    /// How the project shows itself (docs/workspace.md, "Shapes"); `null` for the default.
    pub icon: Option<Value>,
    pub created_at: i64,
    pub resources: Vec<Resource>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Resource {
    pub kind: Kind,
    /// The product's own id (a bucket's name, `owner/name` for a repo).
    pub r#ref: String,
    pub x: f64,
    pub y: f64,
    pub added_at: i64,
}

/// The workspace store.
pub struct Workspace {
    db: Db,
}

impl Workspace {
    /// Open `target` and create the tables if they're missing (every node may do this at once).
    pub fn open(target: &Target, pg: code_sql::PgOptions) -> Result<Workspace> {
        let db = match target {
            Target::Sqlite(path) => Db::open_sqlite(path, code_sql::SqliteOptions { readers: 2, cache_kib: 2 << 10 })?,
            Target::Postgres { url, schema } => Db::connect(url, schema, pg)?,
        };
        Workspace::new(db)
    }

    pub fn new(db: Db) -> Result<Workspace> {
        // Projects before owners (names unique, resources keyed by kind and ref alone): move
        // the old tables aside, create the new ones, and copy them over as the operator's.
        if db.has_column("projects", "id")? && !db.has_column("projects", "owner")? {
            db.migrate("workspace-owners-aside", |c| {
                c.execute_batch("DROP INDEX IF EXISTS resources_project; ALTER TABLE projects RENAME TO projects_v1; ALTER TABLE resources RENAME TO resources_v1;")
            })?;
        }
        db.ensure_schema(SCHEMA)?;
        db.migrate("workspace-owners-copy", |c| {
            if !c.has_column("projects_v1", "id")? {
                return Ok(());
            }
            // Projects made since descriptions and icons came (before owners) keep theirs.
            let look = if c.has_column("projects_v1", "icon")? { "description, icon" } else { "'', NULL" };
            c.execute_batch(&format!(
                "INSERT INTO projects (id, owner, name, description, icon, created_at) SELECT id, '', name, {look}, created_at FROM projects_v1;
                 INSERT INTO resources (owner, kind, ref, project_id, x, y, added_at) SELECT '', kind, ref, project_id, x, y, added_at FROM resources_v1;
                 DROP TABLE projects_v1; DROP TABLE resources_v1;",
            ))
        })?;
        Ok(Workspace { db })
    }

    /// Every project `scope` sees, with its resources, oldest first.
    pub fn list(&self, scope: Scope<'_>) -> Result<Vec<Project>> {
        self.db.read(|c| {
            let projects = c.query(&format!("SELECT {PROJECT_COLUMNS} FROM projects ORDER BY created_at, id"), (), project_row)?;
            let projects: Vec<Project> = projects.into_iter().filter(|p| scope.sees(&p.owner)).collect();
            let resources = c.query(
                "SELECT project_id, kind, ref, x, y, added_at FROM resources ORDER BY added_at, kind, ref",
                (),
                |r| Ok((r.get::<_, String>(0)?, resource_row(r, 1)?)),
            )?;
            let mut by_project: HashMap<String, Vec<Resource>> = HashMap::new();
            for (project, resource) in resources {
                by_project.entry(project).or_default().push(resource);
            }
            Ok(projects
                .into_iter()
                .map(|mut p| {
                    p.resources = by_project.remove(&p.id).unwrap_or_default();
                    p
                })
                .collect())
        })
    }

    pub fn get(&self, scope: Scope<'_>, id: &str) -> Result<Project> {
        self.db.read(|c| {
            owner_of(c, scope, id)?;
            load(c, id)
        })
    }

    /// A new project of `owner` (a user's id, or empty for the operator's).
    pub fn create(&self, owner: &str, name: &str, description: &str, icon: Option<&Value>) -> Result<Project> {
        check_name(name)?;
        check_description(description)?;
        let icon = icon.map(check_icon).transpose()?;
        let id = new_id();
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            name_free(c, owner, name, None)?;
            c.execute(
                "INSERT INTO projects (id, owner, name, description, icon, created_at) VALUES (?, ?, ?, ?, ?, ?)",
                params![id, owner, name, description, icon, now],
            )?;
            load(c, &id)
        })
    }

    /// Change what `change` names; the rest stays.
    pub fn update(&self, scope: Scope<'_>, id: &str, change: &Change) -> Result<Project> {
        if let Some(name) = &change.name {
            check_name(name)?;
        }
        if let Some(d) = &change.description {
            check_description(d)?;
        }
        let icon = match &change.icon {
            Some(Some(v)) => Some(Some(check_icon(v)?)),
            Some(None) => Some(None),
            None => None,
        };
        self.db.write(|c| {
            let owner = owner_of(c, scope, id)?;
            if let Some(name) = &change.name {
                name_free(c, &owner, name, Some(id))?;
                c.execute("UPDATE projects SET name = ? WHERE id = ?", params![name, id])?;
            }
            if let Some(d) = &change.description {
                c.execute("UPDATE projects SET description = ? WHERE id = ?", params![d, id])?;
            }
            if let Some(icon) = &icon {
                c.execute("UPDATE projects SET icon = ? WHERE id = ?", params![icon, id])?;
            }
            load(c, id)
        })
    }

    /// The projects a resource is in (`kind` as stored: `postgres`, `repo`, …).
    pub fn projects_of(&self, kind: &str, r#ref: &str) -> Result<Vec<String>> {
        Ok(self.db.read(|c| c.query("SELECT project_id FROM resources WHERE kind = ? AND ref = ? ORDER BY project_id", [kind, r#ref], |r| r.get(0)))?)
    }

    /// Remove a project and its membership rows (never the resources themselves).
    pub fn delete(&self, scope: Scope<'_>, id: &str) -> Result<()> {
        self.db.write(|c| {
            owner_of(c, scope, id)?;
            c.execute("DELETE FROM resources WHERE project_id = ?", [id])?;
            c.execute("DELETE FROM projects WHERE id = ?", [id])?;
            Ok(())
        })
    }

    /// Put a resource in project `id` at (`x`, `y`): add it, move it on the canvas, or move it
    /// out of another project of the same owner. `added_at` is kept while it stays in the same
    /// project.
    pub fn place(&self, scope: Scope<'_>, id: &str, kind: Kind, r#ref: &str, x: f64, y: f64) -> Result<Resource> {
        check_ref(r#ref)?;
        if !x.is_finite() || !y.is_finite() {
            return Err(Error::BadRequest("x and y must be finite numbers".into()));
        }
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let owner = owner_of(c, scope, id)?;
            let current = c
                .query_row("SELECT project_id, added_at FROM resources WHERE owner = ? AND kind = ? AND ref = ?", [owner.as_str(), kind.as_str(), r#ref], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })
                .optional()?;
            let added_at = match &current {
                Some((project, added_at)) if project == id => *added_at,
                _ => now,
            };
            if current.is_some() {
                c.execute(
                    "UPDATE resources SET project_id = ?, x = ?, y = ?, added_at = ? WHERE owner = ? AND kind = ? AND ref = ?",
                    params![id, x, y, added_at, owner, kind.as_str(), r#ref],
                )?;
            } else {
                c.execute(
                    "INSERT INTO resources (owner, kind, ref, project_id, x, y, added_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
                    params![owner, kind.as_str(), r#ref, id, x, y, added_at],
                )?;
            }
            Ok(Resource { kind, r#ref: r#ref.to_string(), x, y, added_at })
        })
    }

    /// Take a resource out of project `id`.
    pub fn remove(&self, scope: Scope<'_>, id: &str, kind: Kind, r#ref: &str) -> Result<()> {
        self.db.write(|c| {
            owner_of(c, scope, id)?;
            let n = c.execute("DELETE FROM resources WHERE project_id = ? AND kind = ? AND ref = ?", [id, kind.as_str(), r#ref])?;
            match n {
                0 => Err(Error::NotFound(format!("{} {ref:?} isn't in project {id}", kind.as_str()))),
                _ => Ok(()),
            }
        })
    }
}

/// What [`Workspace::update`] changes: `None` leaves a field; `icon: Some(None)` clears the icon.
#[derive(Debug, Default)]
pub struct Change {
    pub name: Option<String>,
    pub description: Option<String>,
    pub icon: Option<Option<Value>>,
}

const PROJECT_COLUMNS: &str = "id, owner, name, description, icon, created_at";

fn project_row(r: &code_sql::Row<'_>) -> code_sql::Result<Project> {
    let icon: Option<String> = r.get(4)?;
    // Only checked icons are stored; one that doesn't parse shows as the default.
    let icon = icon.and_then(|s| serde_json::from_str(&s).ok());
    Ok(Project { id: r.get(0)?, owner: r.get(1)?, name: r.get(2)?, description: r.get(3)?, icon, created_at: r.get(5)?, resources: Vec::new() })
}

/// A resource from columns `kind, ref, x, y, added_at` starting at `at`.
fn resource_row(r: &code_sql::Row<'_>, at: usize) -> code_sql::Result<Resource> {
    let kind: String = r.get(at)?;
    let kind = Kind::parse(&kind).map_err(|e| code_sql::Error::Conversion(e.to_string()))?;
    Ok(Resource { kind, r#ref: r.get(at + 1)?, x: r.get(at + 2)?, y: r.get(at + 3)?, added_at: r.get(at + 4)? })
}

fn load(c: &code_sql::Conn<'_>, id: &str) -> Result<Project> {
    let project = c.query_row(&format!("SELECT {PROJECT_COLUMNS} FROM projects WHERE id = ?"), [id], project_row).optional()?;
    let mut project = project.ok_or_else(|| not_found(id))?;
    project.resources = c.query(
        "SELECT kind, ref, x, y, added_at FROM resources WHERE project_id = ? ORDER BY added_at, kind, ref",
        [id],
        |r| resource_row(r, 0),
    )?;
    Ok(project)
}

/// The owner of project `id`, if `scope` sees it (another owner's project is a 404, as if it
/// didn't exist).
fn owner_of(c: &code_sql::Conn<'_>, scope: Scope<'_>, id: &str) -> Result<String> {
    match c.query_row("SELECT owner FROM projects WHERE id = ?", [id], |r| r.get::<_, String>(0)).optional()? {
        Some(owner) if scope.sees(&owner) => Ok(owner),
        _ => Err(not_found(id)),
    }
}

/// No project of `owner` but `except` is named `name`.
fn name_free(c: &code_sql::Conn<'_>, owner: &str, name: &str, except: Option<&str>) -> Result<()> {
    let taken = c.query_row("SELECT id FROM projects WHERE owner = ? AND name = ?", [owner, name], |r| r.get::<_, String>(0)).optional()?;
    match taken {
        Some(other) if Some(other.as_str()) != except => Err(Error::Conflict(format!("a project named {name} exists"))),
        _ => Ok(()),
    }
}

fn not_found(id: &str) -> Error {
    Error::NotFound(format!("no project {id}"))
}

/// 1–48 characters of `[a-z0-9-]`, not starting or ending with `-`.
fn check_name(name: &str) -> Result<()> {
    let charset = name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if (1..=NAME_MAX).contains(&name.len()) && charset && !name.starts_with('-') && !name.ends_with('-') {
        return Ok(());
    }
    Err(Error::BadRequest(format!(
        "a project name is 1–{NAME_MAX} characters of a-z, 0-9 and -, not starting or ending with -"
    )))
}

fn check_description(d: &str) -> Result<()> {
    match d.chars().count() {
        0..=DESCRIPTION_MAX => Ok(()),
        _ => Err(Error::BadRequest(format!("a description is at most {DESCRIPTION_MAX} characters"))),
    }
}

/// An icon is `{kind, color?}` plus its kind's field: `monogram` (`text`, 1–3 characters, or
/// none for the name's), `emoji` (`emoji`), or `image` (`url`, a PNG, JPEG or WebP `data:` URL
/// up to [`IMAGE_MAX`]; never SVG, which can carry script). Returns it as stored JSON.
fn check_icon(v: &Value) -> Result<String> {
    let bad = |m: &str| Error::BadRequest(format!("icon: {m}"));
    let o: &Map<String, Value> = v.as_object().ok_or_else(|| bad("an object"))?;
    let kind = o.get("kind").and_then(Value::as_str).ok_or_else(|| bad("kind is monogram, emoji or image"))?;
    let field = match kind {
        "monogram" => "text",
        "emoji" => "emoji",
        "image" => "url",
        _ => return Err(bad("kind is monogram, emoji or image")),
    };
    if let Some(k) = o.keys().find(|k| !["kind", "color", field].contains(&k.as_str())) {
        return Err(bad(&format!("unknown field {k}")));
    }
    if let Some(c) = o.get("color")
        && !c.as_str().is_some_and(|c| COLORS.contains(&c))
    {
        return Err(bad(&format!("color is one of {}", COLORS.join(", "))));
    }
    let text = o.get(field).map(|f| f.as_str().ok_or_else(|| bad(&format!("{field} is a string")))).transpose()?;
    match (kind, text) {
        ("monogram", None) => {}
        ("monogram", Some(t)) if (1..=3).contains(&t.chars().count()) && !t.chars().any(char::is_whitespace) => {}
        ("monogram", Some(_)) => return Err(bad("text is 1–3 characters")),
        ("emoji", Some(e)) if (1..=16).contains(&e.chars().count()) && !e.chars().any(|c| c.is_ascii_alphanumeric()) => {}
        ("emoji", _) => return Err(bad("emoji is one emoji")),
        ("image", Some(u)) if u.len() <= IMAGE_MAX && ["png", "jpeg", "webp"].iter().any(|t| u.starts_with(&format!("data:image/{t};base64,"))) => {}
        ("image", Some(u)) if u.len() > IMAGE_MAX => return Err(bad(&format!("the picture is over {} KiB", IMAGE_MAX / 1024))),
        _ => return Err(bad("url is a data:image/png, jpeg or webp base64 URL")),
    }
    Ok(serde_json::to_string(v).expect("JSON"))
}

fn check_ref(r#ref: &str) -> Result<()> {
    match r#ref.chars().count() {
        1..=REF_MAX => Ok(()),
        _ => Err(Error::BadRequest(format!("a resource ref is 1–{REF_MAX} characters"))),
    }
}

/// `prj_` and 12 random characters of lowercase base32.
fn new_id() -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut b = [0u8; 12];
    getrandom::fill(&mut b).expect("system randomness");
    let suffix: String = b.iter().map(|x| ALPHABET[(x & 31) as usize] as char).collect();
    format!("prj_{suffix}")
}

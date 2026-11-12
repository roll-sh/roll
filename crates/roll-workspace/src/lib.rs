//! Workspace projects (docs/workspace.md): product resources grouped into projects, each with its
//! place on the dashboard's canvas.
//!
//! The data is two tables in one `code_sql` database: SQLite at `<home>/workspace.db` on one
//! node, or the system database's `workspace` schema shared by every node. Every read-then-write
//! is one [`Db::write`] closure, so nodes need no leases. Membership only points at resources:
//! nothing here creates or deletes a product's resource.
//!
//! [`api::router`] serves it under `/v1/workspace`.

pub mod api;

use code_sql::{Db, OptionalExtension, Target, params};
use serde::Serialize;
use std::collections::HashMap;

pub use api::router;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS projects (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS resources (
    kind TEXT NOT NULL,
    ref TEXT NOT NULL,
    project_id TEXT NOT NULL,
    x REAL NOT NULL,
    y REAL NOT NULL,
    added_at INTEGER NOT NULL,
    PRIMARY KEY (kind, ref)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS resources_project ON resources (project_id);
";

/// The longest project name.
pub const NAME_MAX: usize = 48;
/// The longest resource ref, in characters.
pub const REF_MAX: usize = 200;

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
    Repo,
}

impl Kind {
    pub const ALL: [Kind; 6] = [Kind::Postgres, Kind::Redis, Kind::Stream, Kind::Bucket, Kind::Nosql, Kind::Repo];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Postgres => "postgres",
            Kind::Redis => "redis",
            Kind::Stream => "stream",
            Kind::Bucket => "bucket",
            Kind::Nosql => "nosql",
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
    pub name: String,
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
        db.ensure_schema(SCHEMA)?;
        Ok(Workspace { db })
    }

    /// Every project with its resources, oldest first.
    pub fn list(&self) -> Result<Vec<Project>> {
        self.db.read(|c| {
            let projects = c.query("SELECT id, name, created_at FROM projects ORDER BY created_at, id", (), project_row)?;
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

    pub fn get(&self, id: &str) -> Result<Project> {
        self.db.read(|c| load(c, id))
    }

    pub fn create(&self, name: &str) -> Result<Project> {
        check_name(name)?;
        let id = new_id();
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            name_free(c, name, None)?;
            c.execute("INSERT INTO projects (id, name, created_at) VALUES (?, ?, ?)", params![id, name, now])?;
            Ok(Project { id: id.clone(), name: name.to_string(), created_at: now, resources: Vec::new() })
        })
    }

    pub fn rename(&self, id: &str, name: &str) -> Result<Project> {
        check_name(name)?;
        self.db.write(|c| {
            exists(c, id)?;
            name_free(c, name, Some(id))?;
            c.execute("UPDATE projects SET name = ? WHERE id = ?", params![name, id])?;
            load(c, id)
        })
    }

    /// Remove a project and its membership rows (never the resources themselves).
    pub fn delete(&self, id: &str) -> Result<()> {
        self.db.write(|c| {
            exists(c, id)?;
            c.execute("DELETE FROM resources WHERE project_id = ?", [id])?;
            c.execute("DELETE FROM projects WHERE id = ?", [id])?;
            Ok(())
        })
    }

    /// Put a resource in project `id` at (`x`, `y`): add it, move it on the canvas, or move it
    /// out of the project it was in. `added_at` is kept while it stays in the same project.
    pub fn place(&self, id: &str, kind: Kind, r#ref: &str, x: f64, y: f64) -> Result<Resource> {
        check_ref(r#ref)?;
        if !x.is_finite() || !y.is_finite() {
            return Err(Error::BadRequest("x and y must be finite numbers".into()));
        }
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            exists(c, id)?;
            let current = c
                .query_row("SELECT project_id, added_at FROM resources WHERE kind = ? AND ref = ?", [kind.as_str(), r#ref], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })
                .optional()?;
            let added_at = match &current {
                Some((project, added_at)) if project == id => *added_at,
                _ => now,
            };
            if current.is_some() {
                c.execute(
                    "UPDATE resources SET project_id = ?, x = ?, y = ?, added_at = ? WHERE kind = ? AND ref = ?",
                    params![id, x, y, added_at, kind.as_str(), r#ref],
                )?;
            } else {
                c.execute(
                    "INSERT INTO resources (kind, ref, project_id, x, y, added_at) VALUES (?, ?, ?, ?, ?, ?)",
                    params![kind.as_str(), r#ref, id, x, y, added_at],
                )?;
            }
            Ok(Resource { kind, r#ref: r#ref.to_string(), x, y, added_at })
        })
    }

    /// Take a resource out of project `id`.
    pub fn remove(&self, id: &str, kind: Kind, r#ref: &str) -> Result<()> {
        self.db.write(|c| {
            let n = c.execute("DELETE FROM resources WHERE project_id = ? AND kind = ? AND ref = ?", [id, kind.as_str(), r#ref])?;
            match n {
                0 => Err(Error::NotFound(format!("{} {ref:?} isn't in project {id}", kind.as_str()))),
                _ => Ok(()),
            }
        })
    }
}

fn project_row(r: &code_sql::Row<'_>) -> code_sql::Result<Project> {
    Ok(Project { id: r.get(0)?, name: r.get(1)?, created_at: r.get(2)?, resources: Vec::new() })
}

/// A resource from columns `kind, ref, x, y, added_at` starting at `at`.
fn resource_row(r: &code_sql::Row<'_>, at: usize) -> code_sql::Result<Resource> {
    let kind: String = r.get(at)?;
    let kind = Kind::parse(&kind).map_err(|e| code_sql::Error::Conversion(e.to_string()))?;
    Ok(Resource { kind, r#ref: r.get(at + 1)?, x: r.get(at + 2)?, y: r.get(at + 3)?, added_at: r.get(at + 4)? })
}

fn load(c: &code_sql::Conn<'_>, id: &str) -> Result<Project> {
    let project = c.query_row("SELECT id, name, created_at FROM projects WHERE id = ?", [id], project_row).optional()?;
    let mut project = project.ok_or_else(|| not_found(id))?;
    project.resources = c.query(
        "SELECT kind, ref, x, y, added_at FROM resources WHERE project_id = ? ORDER BY added_at, kind, ref",
        [id],
        |r| resource_row(r, 0),
    )?;
    Ok(project)
}

fn exists(c: &code_sql::Conn<'_>, id: &str) -> Result<()> {
    match c.query_row("SELECT 1 FROM projects WHERE id = ?", [id], |_| Ok(())).optional()? {
        Some(()) => Ok(()),
        None => Err(not_found(id)),
    }
}

/// No project but `except` is named `name`.
fn name_free(c: &code_sql::Conn<'_>, name: &str, except: Option<&str>) -> Result<()> {
    let taken = c.query_row("SELECT id FROM projects WHERE name = ?", [name], |r| r.get::<_, String>(0)).optional()?;
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

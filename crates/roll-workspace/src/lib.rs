//! Workspace projects (docs/workspace.md): product resources grouped into projects, each with its
//! place on the dashboard's canvas.
//!
//! The data is two tables in one `code_sql` database: SQLite at `<home>/workspace.db` on one
//! node, or the system database's `workspace` schema shared by every node. Every read-then-write
//! is one [`Db::write`] closure, so nodes need no leases. Membership only points at resources:
//! nothing here creates or deletes a product's resource.
//!
//! Every project has a namespace (`owner`): the id of the person who made it (docs/accounts.md), or
//! empty for the operator's. Names and resource places are per namespace. People share a project as
//! its members, each with a role ([`members`], docs/members.md); a [`Scope`] says which projects a
//! caller sees.
//!
//! A deleted project is kept apart (`deleted_projects`, `deleted_resources`, and its members and
//! homes in `deleted_members`, `deleted_homes`) so it can come back ([`Workspace::restore`]) while a
//! product still keeps its data (roll secrets' recovery window), for at most [`KEEP_DELETED_DAYS`].
//! A product may refuse a delete first ([`Workspace::before_delete`]).
//!
//! [`api::router`] serves it under `/v1/workspace`.

pub mod api;
pub mod members;

pub use members::{Actor, Invite, Member, MemberEvent, People, Person, TrustBasis};

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
CREATE TABLE IF NOT EXISTS deleted_projects (
    id TEXT PRIMARY KEY,
    owner TEXT NOT NULL,
    name TEXT NOT NULL,
    description TEXT NOT NULL,
    icon TEXT,
    created_at INTEGER NOT NULL,
    deleted_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS deleted_resources (
    project_id TEXT NOT NULL,
    owner TEXT NOT NULL,
    kind TEXT NOT NULL,
    ref TEXT NOT NULL,
    x REAL NOT NULL,
    y REAL NOT NULL,
    added_at INTEGER NOT NULL,
    PRIMARY KEY (project_id, kind, ref)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS resources_project ON resources (project_id);
";

/// Which projects a caller sees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope<'a> {
    /// Every project: the operator and admins.
    All,
    /// Only the projects this person (a user's id) is a member of.
    Member(&'a str),
}

/// The longest project name.
pub const NAME_MAX: usize = 48;
/// The longest resource ref, in characters.
pub const REF_MAX: usize = 200;
/// The longest description, in characters.
pub const DESCRIPTION_MAX: usize = 280;
/// The biggest picture an icon may carry, as its `data:` URL.
pub const IMAGE_MAX: usize = 200 * 1024;
/// How long a deleted project is kept to be restored, at most (a product drops it sooner with
/// [`Workspace::forget_deleted`]): longer than any product keeps a deleted project's data.
pub const KEEP_DELETED_DAYS: i64 = 91;
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
    #[error("{0}")]
    Forbidden(String),
    /// The change would leave the project without an owner who can sign in.
    #[error("{0}")]
    LastOwner(String),
    /// A product refused a delete ([`Workspace::before_delete`]).
    #[error("{}", .0.message)]
    Refused(Refusal),
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
    Queue,
    Tasks,
    Repo,
    /// A roll apps service (`svc_…`; docs/apps/README.md).
    Service,
}

impl Kind {
    pub const ALL: [Kind; 10] = [Kind::Postgres, Kind::Redis, Kind::Stream, Kind::Bucket, Kind::Nosql, Kind::Search, Kind::Queue, Kind::Tasks, Kind::Repo, Kind::Service];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Postgres => "postgres",
            Kind::Redis => "redis",
            Kind::Stream => "stream",
            Kind::Bucket => "bucket",
            Kind::Nosql => "nosql",
            Kind::Search => "search",
            Kind::Queue => "queue",
            Kind::Tasks => "tasks",
            Kind::Repo => "repo",
            Kind::Service => "service",
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
    /// The caller's role in it, when they're a member.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<roll_auth::roles::Role>,
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

/// A project being deleted, and the request deleting it: what a [`BeforeDelete`] decides on and
/// an [`OnDelete`] is told.
pub struct Deleting<'a> {
    pub project: &'a Project,
    /// rolld's gate put who's asking on its extensions; a product's own confirmation
    /// (`?confirm=`) is in its query.
    pub request: &'a axum::http::request::Parts,
}

/// A product's no to a delete: the status, a stable code and why, answered as they are.
#[derive(Debug)]
pub struct Refusal {
    pub status: u16,
    pub code: String,
    pub message: String,
}

/// Asked before a project is deleted, outside the transaction (roll secrets refuses while the
/// project holds secrets, unless a secrets admin confirms).
pub type BeforeDelete = Box<dyn Fn(&Deleting<'_>) -> std::result::Result<(), Refusal> + Send + Sync>;

/// Called after a project is deleted, outside the transaction (roll secrets keeps its secrets for
/// a recovery window).
pub type OnDelete = Box<dyn Fn(&Deleting<'_>) + Send + Sync>;

/// Who owns a product's resource, as far as a person's project may stand for it: rolld answers
/// from the products' own records (a repo under the person's roll code owner), and an admin's
/// projects stand for anything. Looked up on every use; a failed lookup is a no.
pub trait Owners: Send + Sync {
    /// Whether a project `user` (`usr_…`) made may stand for `kind ref` on its canvas (an admin's:
    /// anything, as the operator's).
    fn trusts(&self, user: &str, kind: Kind, r#ref: &str) -> bool;
    /// Whether `kind ref` is `user`'s own (their repo, their cluster), whatever their account is:
    /// what an owner adds to a project they didn't make, and what a person may place or home. An
    /// admin account is no wider here (docs/members.md, "The canvas rule"). No default: each
    /// implementer says it.
    fn owns(&self, user: &str, kind: Kind, r#ref: &str) -> bool;
}

/// The workspace store.
pub struct Workspace {
    db: Db,
    before_delete: std::sync::RwLock<Vec<BeforeDelete>>,
    on_delete: std::sync::RwLock<Vec<OnDelete>>,
    owners: std::sync::OnceLock<std::sync::Arc<dyn Owners>>,
    people: std::sync::OnceLock<std::sync::Arc<dyn People>>,
    /// roll-access's grants as they are now, for an invite's inviter at accept time.
    grants: std::sync::OnceLock<std::sync::Arc<dyn roll_auth::Grants>>,
    web_url: std::sync::OnceLock<String>,
    on_event: std::sync::RwLock<Vec<members::OnEvent>>,
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
        db.ensure_schema(members::SCHEMA)?;
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
        // Members came after owners: every project's maker is its owner.
        db.migrate("workspace-members", members::migrate)?;
        Ok(Workspace { db, before_delete: Default::default(), on_delete: Default::default(), owners: Default::default(), people: Default::default(), grants: Default::default(), web_url: Default::default(), on_event: Default::default() })
    }

    /// Every project `scope` sees, with its resources, oldest first.
    pub fn list(&self, scope: Scope<'_>) -> Result<Vec<Project>> {
        self.db.read(|c| {
            let projects = c.query(&format!("SELECT {PROJECT_COLUMNS} FROM projects ORDER BY created_at, id"), (), project_row)?;
            let projects: Vec<Project> = match scope {
                Scope::All => projects,
                Scope::Member(user) => {
                    let roles: HashMap<String, String> = c.query("SELECT project_id, role FROM members WHERE user_id = ?", [user], |r| Ok((r.get(0)?, r.get(1)?)))?.into_iter().collect();
                    projects
                        .into_iter()
                        .filter_map(|mut p| {
                            p.role = roll_auth::roles::Role::parse(roles.get(&p.id)?);
                            Some(p)
                        })
                        .collect()
                }
            };
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
            let mut p = load(c, id)?;
            if let Scope::Member(user) = scope {
                p.role = c.query_row("SELECT role FROM members WHERE project_id = ? AND user_id = ?", [id, user], |r| r.get::<_, String>(0)).optional()?.and_then(|r| roll_auth::roles::Role::parse(&r));
            }
            Ok(p)
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
            // After every project before it, even one made the same millisecond: projects list
            // in the order they were made.
            let last: Option<i64> = c.query_row("SELECT MAX(created_at) FROM projects", (), |r| r.get(0))?;
            let at = last.map_or(now, |l| now.max(l + 1));
            c.execute("INSERT INTO projects (id, owner, name, description, icon, created_at) VALUES (?, ?, ?, ?, ?, ?)", params![id, owner, name, description, icon, at])?;
            members::first_owner(c, &id, owner, at)?;
            let mut p = load(c, &id)?;
            p.role = (!owner.is_empty()).then_some(roll_auth::roles::Role::Owner);
            Ok(p)
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

    /// Remove a project, its canvas rows, members and homes (never the resources themselves), once
    /// every [`Workspace::before_delete`] agrees, then tell what [`Workspace::on_delete`]
    /// registered. The project, its canvas, members and homes are kept apart to be restored
    /// ([`Workspace::restore`]); its pending invites end.
    pub fn delete(&self, scope: Scope<'_>, id: &str, request: &axum::http::request::Parts) -> Result<()> {
        let project = self.get(scope, id)?;
        for f in self.before_delete.read().expect("before_delete").iter() {
            f(&Deleting { project: &project, request }).map_err(Error::Refused)?;
        }
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            owner_of(c, scope, id)?;
            let old: Vec<String> = c.query("SELECT id FROM deleted_projects WHERE deleted_at < ? OR id = ?", params![now - KEEP_DELETED_DAYS * 86_400_000, id], |r| r.get(0))?;
            for p in &old {
                forget_deleted_rows(c, p)?;
            }
            c.execute(&format!("INSERT INTO deleted_projects ({PROJECT_COLUMNS}, deleted_at) SELECT {PROJECT_COLUMNS}, ? FROM projects WHERE id = ?"), params![now, id])?;
            c.execute("INSERT INTO deleted_resources (project_id, owner, kind, ref, x, y, added_at) SELECT project_id, owner, kind, ref, x, y, added_at FROM resources WHERE project_id = ?", [id])?;
            c.execute("DELETE FROM resources WHERE project_id = ?", [id])?;
            members::put_aside(c, id)?;
            c.execute("DELETE FROM projects WHERE id = ?", [id])?;
            Ok::<_, Error>(())
        })?;
        for f in self.on_delete.read().expect("on_delete").iter() {
            f(&Deleting { project: &project, request });
        }
        Ok(())
    }

    /// A deleted project as it was, while it's kept ([`KEEP_DELETED_DAYS`]).
    pub fn deleted(&self, id: &str) -> Result<Option<Project>> {
        self.db.read(|c| {
            let Some(mut project) = c.query_row(&format!("SELECT {PROJECT_COLUMNS} FROM deleted_projects WHERE id = ?"), [id], project_row).optional()? else { return Ok(None) };
            project.resources = c.query("SELECT kind, ref, x, y, added_at FROM deleted_resources WHERE project_id = ? ORDER BY added_at, kind, ref", [id], |r| resource_row(r, 0))?;
            Ok(Some(project))
        })
    }

    /// Bring a deleted project back with its canvas, members and homes (a resource since placed in
    /// another project stays there, and a home set elsewhere since stays). 404 if it isn't kept;
    /// 409 if its namespace has another project of its name now. A project that exists is answered
    /// as it is.
    pub fn restore(&self, id: &str) -> Result<Project> {
        self.db.write(|c| {
            if c.query_row("SELECT 1 FROM projects WHERE id = ?", [id], |r| r.get::<_, i64>(0)).optional()?.is_some() {
                return load(c, id);
            }
            let (owner, name): (String, String) = c.query_row("SELECT owner, name FROM deleted_projects WHERE id = ?", [id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?.ok_or_else(|| not_found(id))?;
            name_free(c, &owner, &name, None).map_err(|_| Error::Conflict(format!("a project named {name} exists now: rename it, then restore this one")))?;
            c.execute(&format!("INSERT INTO projects ({PROJECT_COLUMNS}) SELECT {PROJECT_COLUMNS} FROM deleted_projects WHERE id = ?"), [id])?;
            c.execute(
                "INSERT INTO resources (owner, kind, ref, project_id, x, y, added_at) SELECT owner, kind, ref, project_id, x, y, added_at FROM deleted_resources WHERE project_id = ? ON CONFLICT (owner, kind, ref) DO NOTHING",
                [id],
            )?;
            members::bring_back(c, id)?;
            forget_deleted_rows(c, id)?;
            load(c, id)
        })
    }

    /// Stop keeping a deleted project (a product purged what it kept of it).
    pub fn forget_deleted(&self, id: &str) -> Result<()> {
        self.db.write(|c| forget_deleted_rows(c, id))
    }

    /// Whether a deleted project's canvas stood for `kind ref` when it was deleted, from its kept
    /// homes and owners ([`Workspace::trusts`] as it answered then): only for what a product keeps
    /// doing for it through its recovery window (roll secrets' audit sinks), never to decide who
    /// may bring it back.
    pub fn trusts_deleted(&self, project: &Project, kind: Kind, r#ref: &str) -> bool {
        project.owner.is_empty() || self.trusted_when_deleted(&project.id, &project.owner, kind, r#ref)
    }

    /// Ask `f` before each project is deleted, outside the transaction: a [`Refusal`] stops it.
    pub fn before_delete(&self, f: BeforeDelete) {
        self.before_delete.write().expect("before_delete").push(f);
    }

    /// Who owns what a person's canvas holds (set once, by rolld).
    pub fn set_owners(&self, owners: std::sync::Arc<dyn Owners>) {
        let _ = self.owners.set(owners);
    }

    /// Whether `project`'s canvas may stand for `kind ref`: what may act on the resource through the
    /// project (its secrets reaching a repo's checks and previews, a link reaching a database's
    /// credentials). Anyone may put any resource on a canvas, so a person's project stands only for
    /// what was made through it (its home, docs/members.md "The canvas rule") or what [`Owners`]
    /// says is one of its owners'; the operator's projects stand for their canvas. The one check
    /// every consumer asks: never decide this from canvas membership alone.
    pub fn trusts(&self, project: &Project, kind: Kind, r#ref: &str) -> bool {
        project.owner.is_empty() || self.trusted_by_members(&project.id, &project.owner, kind, r#ref)
    }

    /// Whether [`Owners`] says `kind ref` is `user`'s own (their repo, their search cluster): what a
    /// person may put on a canvas, or make a project the home of.
    pub fn owned_by(&self, user: &str, kind: Kind, r#ref: &str) -> bool {
        self.owners.get().is_some_and(|o| o.owns(user, kind, r#ref))
    }

    /// Run `f` after each project is deleted, outside the transaction.
    pub fn on_delete(&self, f: OnDelete) {
        self.on_delete.write().expect("on_delete").push(f);
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
    Ok(Project { id: r.get(0)?, owner: r.get(1)?, name: r.get(2)?, description: r.get(3)?, icon, created_at: r.get(5)?, role: None, resources: Vec::new() })
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

/// The namespace of project `id`, if `scope` sees it (a project the caller isn't a member of is a
/// 404, as if it didn't exist).
pub(crate) fn owner_of(c: &code_sql::Conn<'_>, scope: Scope<'_>, id: &str) -> Result<String> {
    let owner = c.query_row("SELECT owner FROM projects WHERE id = ?", [id], |r| r.get::<_, String>(0)).optional()?;
    let seen = match (scope, &owner) {
        (_, None) => false,
        (Scope::All, Some(_)) => true,
        (Scope::Member(user), Some(_)) => c.query_row("SELECT 1 FROM members WHERE project_id = ? AND user_id = ?", [id, user], |r| r.get::<_, i64>(0)).optional()?.is_some(),
    };
    match owner {
        Some(owner) if seen => Ok(owner),
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

pub(crate) fn not_found(id: &str) -> Error {
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

pub(crate) fn check_ref(r#ref: &str) -> Result<()> {
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

/// A kept deleted project's rows go: the project, its canvas, members and homes.
fn forget_deleted_rows(c: &code_sql::Conn<'_>, id: &str) -> Result<()> {
    c.execute("DELETE FROM deleted_resources WHERE project_id = ?", [id])?;
    members::forget_aside(c, id)?;
    c.execute("DELETE FROM deleted_projects WHERE id = ?", [id])?;
    Ok(())
}

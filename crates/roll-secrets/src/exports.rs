//! Shared values across projects (docs/secrets.md, "Sharing across projects"): a project's admin
//! exports an environment's folder to another project, whose admin then imports it like one of
//! its own (`Import::project`). Both sides consent, both are audited, and removing the export cuts
//! every import of it off at once (it's checked on every read).
//!
//! What crosses is stored values only: plain values and managed credentials. A shared value with
//! a reference, or a live or dynamic link, would resolve against the other project's
//! environments and resources, so it doesn't show through.

use crate::access::{Action, Caller, Scope};
use crate::{Error, Result, Secrets, audit, envs, names};
use code_sql::{Conn, OptionalExtension, params};
use serde::{Deserialize, Serialize};

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS exports (
    env_id TEXT NOT NULL,
    path TEXT NOT NULL,
    to_project TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    PRIMARY KEY (env_id, path, to_project)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS exports_to ON exports (to_project);
";

/// The most projects one folder is shared with.
const EXPORTS_MAX: usize = 50;

/// A folder shared with another project.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Export {
    /// The project it's shared with.
    pub project: String,
    #[serde(default, skip_deserializing)]
    pub created_at: i64,
    #[serde(default, skip_deserializing)]
    pub created_by: String,
}

/// Whether `from_env_id`'s `path` is shared with `to_project` (inside a read or a write).
pub(crate) fn exported(c: &Conn<'_>, from_env_id: &str, path: &str, to_project: &str) -> Result<bool> {
    Ok(c.query_row("SELECT 1 FROM exports WHERE env_id = ? AND path = ? AND to_project = ?", [from_env_id, path, to_project], |r| r.get::<_, i64>(0)).optional()?.is_some())
}

impl Secrets {
    /// The folders other projects share with `project`, ready to import (`Import`s).
    pub fn shared_with(&self, project: &str, caller: &Caller) -> Result<Vec<crate::Import>> {
        caller.need(Action::Read, &Scope::project(project))?;
        self.project(project)?;
        self.db.read(|c| {
            Ok(c.query(
                "SELECT e.project_id, e.slug, x.path FROM exports x JOIN environments e ON e.id = x.env_id WHERE x.to_project = ? ORDER BY e.project_id, e.slug, x.path",
                [project],
                |r| Ok(crate::Import { project: Some(r.get(0)?), environment: r.get(1)?, path: r.get(2)? }),
            )?)
        })
    }

    /// The projects `slug`'s `path` is shared with.
    pub fn exports(&self, project: &str, slug: &str, path: &str, caller: &Caller) -> Result<Vec<Export>> {
        let path = names::path(path)?;
        caller.need(Action::Read, &Scope::at(project, slug, &path))?;
        self.project(project)?;
        self.db.read(|c| {
            let env = envs::by_slug(c, project, slug)?;
            Ok(c.query("SELECT to_project, created_at, created_by FROM exports WHERE env_id = ? AND path = ? ORDER BY created_at", [&env.id, &path], |r| {
                Ok(Export { project: r.get(0)?, created_at: r.get(1)?, created_by: r.get(2)? })
            })?)
        })
    }

    /// Replace the projects `slug`'s `path` is shared with (an admin's: it hands the folder's
    /// values to every reader of those projects' importing folders).
    pub fn set_exports(&self, project: &str, slug: &str, path: &str, list: Vec<Export>, caller: &Caller) -> Result<Vec<Export>> {
        let path = names::path(path)?;
        caller.need(Action::Admin, &Scope::at(project, slug, &path))?;
        if list.len() > EXPORTS_MAX {
            return Err(Error::BadRequest(format!("a folder is shared with at most {EXPORTS_MAX} projects")));
        }
        let mut to: Vec<String> = list.into_iter().map(|e| e.project).collect();
        to.sort();
        to.dedup();
        for p in &to {
            if p == project {
                return Err(Error::BadRequest("a folder is shared with other projects (its own environments import it directly)".into()));
            }
            // A project the caller can't see is as missing as one that doesn't exist.
            if !caller.grants.sees(p) {
                return Err(Error::BadRequest(format!("no project {p}")));
            }
            self.workspace.get(roll_workspace::Scope::All, p).map_err(|_| Error::BadRequest(format!("no project {p}")))?;
        }
        self.project(project)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let env = envs::by_slug(c, project, slug)?;
            c.execute("DELETE FROM exports WHERE env_id = ? AND path = ?", [&env.id, &path])?;
            for p in &to {
                c.execute("INSERT INTO exports (env_id, path, to_project, created_at, created_by) VALUES (?, ?, ?, ?, ?)", params![env.id, path, p, now, caller.principal])?;
            }
            let summary = if to.is_empty() { "shared with no project".to_string() } else { format!("shared with {}", to.join(", ")) };
            audit::record(c, project, caller, "exports.set", Some(slug), Some(&path), &[], &summary, now)?;
            Ok(c.query("SELECT to_project, created_at, created_by FROM exports WHERE env_id = ? AND path = ? ORDER BY created_at", [&env.id, &path], |r| {
                Ok(Export { project: r.get(0)?, created_at: r.get(1)?, created_by: r.get(2)? })
            })?)
        })
    }
}

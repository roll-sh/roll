//! Required keys (docs/secrets.md, "Required keys"): keys every standard environment of a project
//! must define in a folder. The grid flags one an environment lacks, and reading an environment's
//! values (`roll run`, an app's start, a check) reports it as a problem.

use crate::access::{Action, Caller, Scope};
use crate::{Error, Result, Secrets, audit, names};
use code_sql::{Conn, params};
use serde::{Deserialize, Serialize};

/// The most keys one project can require.
pub const MAX: usize = 500;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Required {
    pub key: String,
    #[serde(default = "root")]
    pub path: String,
    #[serde(default)]
    pub comment: String,
}

fn root() -> String {
    "/".into()
}

pub(crate) fn of(c: &Conn<'_>, project: &str) -> Result<Vec<Required>> {
    Ok(c.query("SELECT key, path, comment FROM required WHERE project_id = ? ORDER BY path, key", [project], |r| Ok(Required { key: r.get(0)?, path: r.get(1)?, comment: r.get(2)? }))?)
}

impl Secrets {
    pub fn required(&self, project: &str, caller: &Caller) -> Result<Vec<Required>> {
        caller.need(Action::Read, &Scope::project(project))?;
        self.project(project)?;
        self.db.read(|c| of(c, project))
    }

    /// Replace the project's required keys.
    pub fn set_required(&self, project: &str, list: Vec<Required>, caller: &Caller) -> Result<Vec<Required>> {
        caller.need(Action::Admin, &Scope::project(project))?;
        if list.len() > MAX {
            return Err(Error::BadRequest(format!("at most {MAX} required keys")));
        }
        let mut list = list;
        for r in &mut list {
            names::key(&r.key)?;
            r.path = names::path(&r.path)?;
            names::comment(&r.comment)?;
        }
        list.sort_by(|a, b| (&a.path, &a.key).cmp(&(&b.path, &b.key)));
        list.dedup_by(|a, b| a.path == b.path && a.key == b.key);
        self.project(project)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            c.execute("DELETE FROM required WHERE project_id = ?", [project])?;
            for r in &list {
                c.execute("INSERT INTO required (project_id, path, key, comment, created_at) VALUES (?, ?, ?, ?, ?)", params![project, r.path, r.key, r.comment, now])?;
            }
            let keys: Vec<String> = list.iter().map(|r| r.key.clone()).collect();
            audit::record(c, project, caller, "required.set", None, None, &keys, &format!("{} required", crate::write::plural(list.len(), "key")), now)?;
            of(c, project)
        })
    }

    /// The keys `path` requires that `have` lacks.
    pub(crate) fn missing_required(&self, project: &str, path: &str, have: &[String]) -> Result<Vec<Required>> {
        let all = self.db.read(|c| of(c, project))?;
        Ok(all.into_iter().filter(|r| r.path == path && !have.contains(&r.key)).collect())
    }
}

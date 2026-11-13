//! Finding a key across projects (docs/secrets.md, "Finding a key"): every project, environment
//! and folder that holds it, among those the caller may read. Names, versions and whether it's
//! linked; never a value. For rotating an organisation-wide key: "where is STRIPE_KEY?".

use crate::access::{Action, Caller, Scope};
use crate::{Error, Result, Secrets, names};
use serde::Serialize;

/// The most places one search answers.
const FOUND_MAX: usize = 500;

#[derive(Debug, Serialize)]
pub struct Place {
    pub project: String,
    pub project_name: String,
    pub environment: String,
    pub path: String,
    pub key: String,
    pub version: i64,
    pub updated_at: i64,
    pub linked: bool,
}

impl Secrets {
    /// Where `key` is set (`PREFIX*` for every key starting with `PREFIX`). Personal values aren't
    /// places: only shared ones.
    pub fn find(&self, key: &str, caller: &Caller) -> Result<Vec<Place>> {
        let (exact, prefix) = match key.strip_suffix('*') {
            Some(p) => (None, Some(p)),
            None => (Some(key), None),
        };
        let probe = exact.or(prefix).unwrap_or_default();
        if probe.is_empty() || (exact.is_some() && names::key(probe).is_err()) || !probe.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(Error::BadRequest("key is a key's name, or the start of one followed by *".into()));
        }
        type Row = (String, String, String, String, i64, i64, Option<String>);
        let rows: Vec<Row> = self.db.read(|c| {
            let sql = "SELECT s.project_id, e.slug, s.path, s.key, s.version, s.updated_at, s.link FROM secrets s JOIN environments e ON e.id = s.env_id WHERE s.owner = '' AND ";
            Ok::<_, Error>(match exact {
                Some(k) => c.query(&format!("{sql}s.key = ? ORDER BY s.project_id, e.position, s.path LIMIT 5000"), [k], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?,
                // Keys are [A-Za-z0-9_]: `_` is LIKE's one-character wildcard, so escape it.
                None => c.query(&format!("{sql}s.key LIKE ? ESCAPE '\\' ORDER BY s.project_id, e.position, s.path, s.key LIMIT 5000"), [format!("{}%", probe.replace('_', "\\_"))], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
                })?,
            })
        })?;
        let mut names_of = std::collections::HashMap::new();
        let mut out = vec![];
        for (project, env, path, key, version, updated_at, link) in rows {
            if out.len() >= FOUND_MAX {
                break;
            }
            if !caller.allows(Action::Read, &Scope::at(&project, &env, &path)) {
                continue;
            }
            let project_name = names_of.entry(project.clone()).or_insert_with(|| self.workspace.get(&project).map(|p| p.name).unwrap_or_else(|_| project.clone())).clone();
            out.push(Place { project, project_name, environment: env, path, key, version, updated_at, linked: link.is_some() });
        }
        Ok(out)
    }
}

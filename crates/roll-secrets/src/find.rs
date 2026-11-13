//! Finding a key across projects (docs/secrets.md, "Finding a key"): every project, environment
//! and folder that holds it, among those the caller may read. Names, versions and whether it's
//! linked; never a value. For rotating an organisation-wide key: "where is STRIPE_KEY?".

use code_sql::params;
use crate::access::{Action, Caller, Scope};
use crate::{Error, Result, Secrets, names};
use serde::Serialize;

/// The most places one search answers.
const FOUND_MAX: usize = 500;
/// Rows read a page at a time, and at most this many in all.
const PAGE: i64 = 5000;
const SCANNED_MAX: i64 = 100_000;

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
        // A caller bound to one project (a token) is looked for there, before the window: other
        // projects' rows must not crowd its own out.
        let only = caller.grants.project().map(String::from);
        let (matching, arg) = match exact {
            Some(k) => ("s.key = ?", k.to_string()),
            // Keys are [A-Za-z0-9_]: `_` is LIKE's one-character wildcard, so escape it.
            None => ("s.key LIKE ? ESCAPE '\\'", format!("{}%", probe.replace('_', "\\_"))),
        };
        let sql = format!(
            "SELECT s.project_id, e.slug, s.path, s.key, s.version, s.updated_at, s.link FROM secrets s JOIN environments e ON e.id = s.env_id
             WHERE s.owner = '' AND {matching} AND (CAST(? AS TEXT) IS NULL OR s.project_id = ?) ORDER BY s.project_id, e.position, s.path, s.key LIMIT ? OFFSET ?"
        );
        let mut names_of = std::collections::HashMap::new();
        let mut out = vec![];
        // Pages of rows, filtered by what the caller may read, until enough places or the end:
        // rows it can't read never crowd out ones it can.
        for page in 0..(SCANNED_MAX / PAGE) {
            let rows: Vec<Row> = self.db.read(|c| Ok::<_, Error>(c.query(&sql, params![arg, only, only, PAGE, page * PAGE], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?))?;
            let last = (rows.len() as i64) < PAGE;
            for (project, env, path, key, version, updated_at, link) in rows {
                if out.len() >= FOUND_MAX {
                    return Ok(out);
                }
                if !caller.allows(Action::Read, &Scope::at(&project, &env, &path)) {
                    continue;
                }
                let project_name = names_of.entry(project.clone()).or_insert_with(|| self.workspace.get(&project).map(|p| p.name).unwrap_or_else(|_| project.clone())).clone();
                out.push(Place { project, project_name, environment: env, path, key, version, updated_at, linked: link.is_some() });
            }
            if last {
                break;
            }
        }
        Ok(out)
    }

    /// Of `keys`, the ones a standard environment of `project` doesn't define, with those
    /// environments (roll code's notice that a change starts reading one: names only). Defined
    /// means set in any of its folders, or reaching its root through an import or its base.
    pub fn keys_missing(&self, project: &str, keys: &[String]) -> Result<Vec<(String, Vec<String>)>> {
        let operator = Caller::operator();
        let envs: Vec<crate::envs::EnvRow> = self.db.read(|c| crate::envs::all(c, project))?.into_iter().filter(|e| e.kind == crate::envs::Kind::Standard).collect();
        let mut missing: std::collections::BTreeMap<String, Vec<String>> = Default::default();
        for e in &envs {
            let mut have: std::collections::HashSet<String> = self.db.read(|c| c.query("SELECT DISTINCT key FROM secrets WHERE env_id = ? AND owner = ''", [&e.id], |r| r.get::<_, String>(0)))?.into_iter().collect();
            have.extend(self.view(project, &e.slug, "/", &operator, true, Action::Read)?.into_iter().map(|x| x.row.key));
            for k in keys.iter().filter(|k| !have.contains(*k)) {
                missing.entry(k.clone()).or_default().push(e.slug.clone());
            }
        }
        Ok(missing.into_iter().collect())
    }

    /// A local copy (a `.env` file's entries) against a folder, by fingerprint: which keys it adds,
    /// changes or keeps the same, and which it lacks. Names only; it needs `reveal` (comparing is
    /// a way to test a guess) and is audited. A value with references, or a link's, can't be
    /// compared with a copy: those are listed apart.
    pub fn compare_file(&self, project: &str, slug: &str, path: &str, entries: &[crate::dotenv::Entry], caller: &Caller) -> Result<FileDiff> {
        let path = names::path(path)?;
        caller.need(Action::Reveal, &Scope::at(project, slug, &path))?;
        let dek = self.project(project)?;
        let have: std::collections::BTreeMap<String, crate::read::Entry> = self.view(project, slug, &path, caller, false, Action::Reveal)?.into_iter().filter(|e| e.row.owner.is_empty()).map(|e| (e.row.key.clone(), e)).collect();
        let mut d = FileDiff::default();
        let local: std::collections::BTreeMap<&str, &crate::dotenv::Entry> = entries.iter().map(|e| (e.key.as_str(), e)).collect();
        for (key, e) in &local {
            match have.get(*key) {
                None => d.added.push(key.to_string()),
                Some(x) if x.row.link.is_some() => d.linked.push(key.to_string()),
                Some(x) if x.row.refs.as_ref().is_some_and(|r| !r.is_empty()) => d.references.push(key.to_string()),
                Some(x) if dek.fingerprint(e.value.expose()) == x.row.fp => d.same.push(key.to_string()),
                Some(_) => d.changed.push(key.to_string()),
            }
        }
        d.missing = have.keys().filter(|k| !local.contains_key(k.as_str())).cloned().collect();
        let keys: Vec<String> = local.keys().map(|k| k.to_string()).collect();
        self.audit_now(project, caller, "secrets.compare", Some(slug), Some(&path), &keys, &format!("compared a local copy of {} keys", keys.len()));
        Ok(d)
    }
}

/// A local copy against a folder (`POST …/environments/{env}/diff`): names only.
#[derive(Debug, Default, Serialize)]
pub struct FileDiff {
    /// In the file, not in the folder.
    pub added: Vec<String>,
    /// In both, with a different value.
    pub changed: Vec<String>,
    pub same: Vec<String>,
    /// In the folder, not in the file.
    pub missing: Vec<String>,
    /// Values with references and links' values, which a copy can't be compared with.
    pub references: Vec<String>,
    pub linked: Vec<String>,
}

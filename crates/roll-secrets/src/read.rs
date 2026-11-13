//! Reading (docs/secrets.md, "References", "Imports", "Compare and promote"): a folder's
//! effective secrets are its own, then its imports', then its base environment's; values are
//! decrypted, links read or issued, and references resolved here.

use crate::access::{Action, Caller, Scope};
use crate::crypto::{Dek, value_aad};
use crate::envs::{self, EnvRow, Kind};
use crate::links::{Link, Mode};
use crate::refs::{self, Part};
use crate::write::{Item, ItemAction, SecretType};
use crate::{Error, Result, Secrets, Value, from_json, json, names};
use code_sql::{Conn, params};
use serde::Serialize;
use serde_json::Value as Json;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// How deep references and imports are followed.
pub const DEPTH: usize = 8;

#[derive(Clone, Debug)]
pub(crate) struct Row {
    pub id: String,
    /// Its project: the one asked about, or another that shares the folder (exports.rs).
    pub project_id: String,
    pub env_id: String,
    pub path: String,
    pub key: String,
    pub owner: String,
    pub version: i64,
    pub value: Option<Vec<u8>>,
    pub fp: String,
    pub comment: String,
    pub tags: Vec<String>,
    pub link: Option<Link>,
    pub updated_at: i64,
    pub updated_by: String,
    /// The references in the value, as written (`None`: a row from before they were kept).
    pub refs: Option<Vec<String>>,
}

impl Row {
    /// The references in its value, decrypting only a row from before they were kept.
    fn references(&self, dek: &Dek, project: &str) -> Vec<String> {
        match (&self.refs, &self.value) {
            (Some(r), _) => r.clone(),
            (None, Some(ct)) => dek.open(&value_aad(project, &self.id, self.version), ct).map(|v| refs::refs(v.expose()).iter().map(refs::Ref::display).collect()).unwrap_or_default(),
            (None, None) => vec![],
        }
    }
}

const ROW_COLUMNS: &str = "id, env_id, path, key, owner, version, value, fp, comment, tags, link, updated_at, updated_by, refs, project_id";

fn row(r: &code_sql::Row<'_>) -> code_sql::Result<Row> {
    Ok(Row {
        id: r.get(0)?,
        env_id: r.get(1)?,
        path: r.get(2)?,
        key: r.get(3)?,
        owner: r.get(4)?,
        version: r.get(5)?,
        value: r.get(6)?,
        fp: r.get(7)?,
        comment: r.get(8)?,
        tags: from_json(&r.get::<_, String>(9)?),
        link: r.get::<_, Option<String>>(10)?.and_then(|s| serde_json::from_str(&s).ok()),
        updated_at: r.get(11)?,
        updated_by: r.get(12)?,
        refs: r.get::<_, Option<String>>(13)?.and_then(|s| serde_json::from_str(&s).ok()),
        project_id: r.get(14)?,
    })
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Source {
    Own,
    Import {
        environment: String,
        path: String,
        /// Another project that shares the folder.
        #[serde(skip_serializing_if = "Option::is_none")]
        project: Option<String>,
    },
    Base { environment: String },
}

#[derive(Clone, Debug)]
pub(crate) struct Entry {
    pub row: Row,
    /// The slug of the environment the row is in.
    pub env: String,
    pub source: Source,
    /// A personal value of the caller replaces a shared one.
    pub overridden: bool,
}

/// A secret as the API shows it, without its value.
#[derive(Clone, Debug, Serialize)]
pub struct Secret {
    pub id: String,
    pub key: String,
    pub path: String,
    #[serde(rename = "type")]
    pub kind: SecretType,
    pub version: i64,
    pub comment: String,
    pub tags: Vec<String>,
    pub link: Option<Link>,
    pub fp: String,
    /// The references in its value (`staging.DB_URL`).
    pub references: Vec<String>,
    pub updated_at: i64,
    pub updated_by: String,
    pub source: Source,
    pub overridden: bool,
}

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct Problem {
    pub key: String,
    pub problem: String,
}

/// Resolved values (`…/values`, `…/resolve`).
#[derive(Debug, Serialize)]
pub struct Values {
    pub values: BTreeMap<String, Value>,
    pub problems: Vec<Problem>,
    pub leases: Vec<String>,
    /// Keys whose value is a dynamic link's short-lived credential (it ends with its lease: a
    /// consumer that can't renew it, such as a service that scales to zero, should refuse them).
    pub dynamic: Vec<String>,
    /// Keys whose value comes from a link to a resource (any mode): a consumer that reaches
    /// resources another way (a container on a private network) knows which addresses to rewrite.
    pub linked: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Cell {
    pub fp: String,
    pub version: i64,
    pub updated_at: i64,
    pub link: Option<Link>,
    pub reference: bool,
    pub source: &'static str,
    pub comment: bool,
    pub personal: bool,
    /// The typed-key rule its stored value breaks (`PORT must be a port (1–65535)`), for a caller
    /// who may reveal it; a value with a reference is checked when it's resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub misfit: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct GridRow {
    pub key: String,
    pub cells: BTreeMap<String, Option<Cell>>,
    pub status: &'static str,
    /// Every standard environment must define it (`…/required`).
    pub required: bool,
}

#[derive(Debug, Serialize)]
pub struct Grid {
    pub path: String,
    pub environments: Vec<String>,
    pub rows: Vec<GridRow>,
    pub folders: Vec<String>,
    pub previews: i64,
}

#[derive(Debug, Serialize)]
pub struct Version {
    pub version: i64,
    pub seq: i64,
    pub op: String,
    pub what: Vec<String>,
    pub fp: Option<String>,
    pub comment: String,
    pub tags: Vec<String>,
    pub link: Option<Link>,
    pub at: i64,
    pub by: String,
    /// Its value was purged (it can't be revealed or rolled back to).
    pub purged: bool,
}

/// Whether entries of another environment's folder may show through an import or a base.
type Through<'a> = &'a dyn Fn(&str, &str) -> bool;

/// The effective entries of `env`/`path` for `principal` (`None`: shared values only). Imports
/// and the base count only where `through(environment, path)` allows: a caller never reads an
/// environment through another that it couldn't read itself.
#[allow(clippy::too_many_arguments)]
fn view_in(c: &Conn<'_>, env: &EnvRow, path: &str, principal: Option<&str>, through: Through<'_>, untrusted: bool, depth: usize, seen: &mut HashSet<(String, String)>) -> Result<Vec<Entry>> {
    if depth > DEPTH || !seen.insert((env.id.clone(), path.to_string())) {
        return Ok(vec![]);
    }
    let owner = principal.unwrap_or("");
    let rows = c.query(&format!("SELECT {ROW_COLUMNS} FROM secrets WHERE env_id = ? AND path = ? AND (owner = '' OR owner = ?) ORDER BY key, owner"), [&env.id, path, owner], row)?;
    let mut out: Vec<Entry> = vec![];
    for r in rows {
        if !r.owner.is_empty() {
            if let Some(shared) = out.iter_mut().find(|e| e.row.key == r.key) {
                *shared = Entry { row: r, env: env.slug.clone(), source: Source::Own, overridden: true };
                continue;
            }
            out.push(Entry { row: r, env: env.slug.clone(), source: Source::Own, overridden: false });
        } else {
            out.push(Entry { row: r, env: env.slug.clone(), source: Source::Own, overridden: false });
        }
    }
    let mut have: HashSet<String> = out.iter().map(|e| e.row.key.clone()).collect();
    for imp in envs::imports_of(c, &env.id, path)? {
        let entries = match &imp.project {
            None => {
                let Ok(from) = envs::by_slug(c, &env.project_id, &imp.environment) else { continue };
                if !through(&from.slug, &imp.path) {
                    continue;
                }
                view_in(c, &from, &imp.path, principal, through, untrusted, depth + 1, seen)?
            }
            // Another project's folder, while it's shared with this one: its own stored values
            // (no personal ones, nothing it imports, no reference or live link, which would
            // resolve against that project). The share stands in for the caller's access there.
            Some(p) => {
                let Ok(from) = envs::by_slug(c, p, &imp.environment) else { continue };
                // An untrusted consumer never reads a protected environment, shared or not.
                if (untrusted && from.protected) || !crate::exports::exported(c, &from.id, &imp.path, &env.project_id)? {
                    continue;
                }
                c.query(&format!("SELECT {ROW_COLUMNS} FROM secrets WHERE env_id = ? AND path = ? AND owner = '' ORDER BY key"), [&from.id, &imp.path], row)?
                    .into_iter()
                    .filter(|r| r.refs.as_ref().is_some_and(|x| x.is_empty()) && r.link.as_ref().is_none_or(|l| l.mode == Mode::Managed))
                    .map(|r| Entry { row: r, env: from.slug.clone(), source: Source::Own, overridden: false })
                    .collect()
            }
        };
        for mut e in entries {
            if have.insert(e.row.key.clone()) {
                if e.source == Source::Own {
                    e.source = Source::Import { environment: imp.environment.clone(), path: imp.path.clone(), project: imp.project.clone() };
                }
                out.push(e);
            }
        }
    }
    if let Some(base) = &env.base_id {
        let base = envs::by_id(c, base)?;
        let inherited = if through(&base.slug, path) { view_in(c, &base, path, principal, through, untrusted, depth + 1, seen)? } else { vec![] };
        for mut e in inherited {
            if have.insert(e.row.key.clone()) {
                if e.source == Source::Own {
                    e.source = Source::Base { environment: base.slug.clone() };
                }
                out.push(e);
            }
        }
    }
    out.sort_by(|a, b| a.row.key.cmp(&b.row.key));
    Ok(out)
}

impl Secrets {
    /// The effective entries of a folder; `shared` leaves out the caller's personal values.
    /// Imports and the base show through only where the caller may `action` them.
    pub(crate) fn view(&self, project: &str, slug: &str, path: &str, caller: &Caller, shared: bool, action: Action) -> Result<Vec<Entry>> {
        let principal = (!shared && caller.personal()).then_some(caller.principal.as_str());
        let through = |e: &str, p: &str| caller.allows(action, &Scope::at(project, e, p));
        self.db.read(|c| {
            let env = envs::by_slug(c, project, slug)?;
            view_in(c, &env, path, principal, &through, caller.grants.untrusted(), 0, &mut HashSet::new())
        })
    }

    /// `e` as the API shows it. Its fingerprint only to a caller who may reveal it: a writer
    /// could otherwise test guesses against values it can't read.
    fn secret_of(&self, dek: &Dek, project: &str, e: Entry, caller: &Caller) -> Secret {
        let fp = if caller.allows(Action::Reveal, &Scope::at(project, &e.env, &e.row.path)) { e.row.fp.clone() } else { String::new() };
        let references = e.row.references(dek, project);
        Secret {
            id: e.row.id,
            key: e.row.key,
            path: e.row.path,
            kind: if e.row.owner.is_empty() { SecretType::Shared } else { SecretType::Personal },
            version: e.row.version,
            comment: e.row.comment,
            tags: e.row.tags,
            link: e.row.link,
            fp,
            references,
            updated_at: e.row.updated_at,
            updated_by: e.row.updated_by,
            source: e.source,
            overridden: e.overridden,
        }
    }

    /// A folder's effective secrets, without values.
    pub fn secrets(&self, project: &str, slug: &str, path: &str, caller: &Caller) -> Result<Vec<Secret>> {
        let path = names::path(path)?;
        caller.need(Action::Read, &Scope::at(project, slug, &path))?;
        let dek = self.project(project)?;
        let entries = self.view(project, slug, &path, caller, false, Action::Read)?;
        Ok(entries.into_iter().map(|e| self.secret_of(&dek, project, e, caller)).collect())
    }

    /// One effective secret, without its value. `kind`: the shared one or the caller's personal
    /// one (default: whichever the caller reads).
    pub fn secret(&self, project: &str, slug: &str, path: &str, key: &str, kind: Option<SecretType>, caller: &Caller) -> Result<Secret> {
        let path = names::path(path)?;
        caller.need(Action::Read, &Scope::at(project, slug, &path))?;
        let dek = self.project(project)?;
        let entries = self.view(project, slug, &path, caller, kind == Some(SecretType::Shared), Action::Read)?;
        let e = entries.into_iter().find(|e| e.row.key == key).ok_or_else(|| Error::NotFound(format!("no secret {key} in {slug}:{path}")))?;
        if kind == Some(SecretType::Personal) && e.row.owner.is_empty() {
            return Err(Error::NotFound(format!("no personal value of {key} in {slug}:{path}")));
        }
        Ok(self.secret_of(&dek, project, e, caller))
    }

    /// One secret's value, resolved unless `resolve` is false. Audited.
    pub fn reveal(&self, project: &str, slug: &str, path: &str, key: &str, resolve: bool, caller: &Caller) -> Result<(Value, Vec<Problem>)> {
        let path = names::path(path)?;
        caller.need(Action::Reveal, &Scope::at(project, slug, &path))?;
        let narrowed = self.caller_for(project, slug, caller)?;
        let caller = &narrowed;
        let dek = self.project(project)?;
        let entries = self.view(project, slug, &path, caller, false, Action::Reveal)?;
        let e = entries.iter().find(|e| e.row.key == key).ok_or_else(|| Error::NotFound(format!("no secret {key} in {slug}:{path}")))?;
        let mut r = Resolver::new(self, project, &dek, caller, resolve, &caller.principal);
        let (v, problem) = r.entry(e);
        let leases = r.finish(slug)?;
        self.audit_now(project, caller, "secrets.reveal", Some(slug), Some(&path), &[key.to_string()], &format!("revealed {key}{}", lease_note(&leases)));
        Ok((v, problem.into_iter().map(|p| Problem { key: key.into(), problem: p }).collect()))
    }

    /// Every effective value of a folder (`…/values`, `roll run`, an app's start). Audited as
    /// `action` by `consumer`.
    pub fn values(&self, project: &str, slug: &str, path: &str, resolve: bool, consumer: Option<&str>, caller: &Caller) -> Result<Values> {
        let path = names::path(path)?;
        caller.need(Action::Reveal, &Scope::at(project, slug, &path))?;
        let narrowed = self.caller_for(project, slug, caller)?;
        let caller = &narrowed;
        let dek = self.project(project)?;
        let entries = self.view(project, slug, &path, caller, false, Action::Reveal)?;
        let who = consumer.unwrap_or(&caller.principal).to_string();
        let mut r = Resolver::new(self, project, &dek, caller, resolve, &who);
        let mut values = BTreeMap::new();
        let mut problems = vec![];
        for e in &entries {
            let (v, problem) = r.entry(e);
            if let Some(p) = problem {
                problems.push(Problem { key: e.row.key.clone(), problem: p });
            }
            values.insert(e.row.key.clone(), v);
        }
        let leases = r.finish(slug)?;
        let have: Vec<String> = values.keys().cloned().collect();
        for m in self.missing_required(project, &path, &have)? {
            problems.push(Problem { key: m.key, problem: format!("required, but {slug} doesn't set it") });
        }
        if resolve {
            let typed: Vec<Problem> = self.type_problems(project, &path, &values)?.into_iter().filter(|t| !problems.iter().any(|p| p.key == t.key)).collect();
            problems.extend(typed);
        }
        let action = if consumer.is_some() { "secrets.resolve" } else { "secrets.export" };
        let keys: Vec<String> = values.keys().cloned().collect();
        let summary = match consumer {
            Some(c) => format!("resolved {} for {c}{}", crate::write::plural(keys.len(), "value"), lease_note(&leases)),
            None => format!("exported {}{}", crate::write::plural(keys.len(), "value"), lease_note(&leases)),
        };
        self.audit_now(project, caller, action, Some(slug), Some(&path), &keys, &summary);
        let dynamic = entries.iter().filter(|e| e.row.link.as_ref().is_some_and(|l| l.mode == Mode::Dynamic)).map(|e| e.row.key.clone()).collect();
        let linked = entries.iter().filter(|e| e.row.link.is_some()).map(|e| e.row.key.clone()).collect();
        Ok(Values { values, problems, leases, dynamic, linked })
    }

    /// A digest of a folder's effective secrets (keys, where they come from, versions,
    /// fingerprints, links): it changes when what `values` would return changes, except a live
    /// link's resource rotating. Decrypts nothing, issues nothing, isn't audited: what
    /// `roll run --watch` polls.
    pub fn digest(&self, project: &str, slug: &str, path: &str, caller: &Caller) -> Result<String> {
        use sha2::{Digest, Sha256};
        let path = names::path(path)?;
        caller.need(Action::Read, &Scope::at(project, slug, &path))?;
        // As the values are read: an untrusted ephemeral environment's digest leaves its protected
        // base out, so a deploy compares like with like.
        let narrowed = self.caller_for(project, slug, caller)?;
        let caller = &narrowed;
        self.project(project)?;
        let entries = self.view(project, slug, &path, caller, false, Action::Reveal)?;
        let dek = self.project(project)?;
        let mut h = Sha256::new();
        // No fingerprint: a digest is readable without reveal, and a version moves with every value.
        let stamp = |h: &mut Sha256, e: &Entry| h.update(format!("{}\0{}\0{}\0{}\0{}\n", e.row.key, e.env, e.row.id, e.row.version, e.row.link.as_ref().map(json).unwrap_or_default()));
        for e in &entries {
            stamp(&mut h, e);
        }
        // What references reach, in this environment or another, moves the digest too.
        let mut views: HashMap<(String, String), Vec<Entry>> = HashMap::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut todo: Vec<(Entry, usize)> = entries.iter().map(|e| (e.clone(), 0)).collect();
        while let Some((e, depth)) = todo.pop() {
            if depth >= DEPTH {
                continue;
            }
            for display in e.row.references(&dek, project) {
                let Some(r) = refs::refs(&format!("${{{display}}}")).into_iter().next() else { continue };
                let (env, at) = match &r.env {
                    None => (e.env.clone(), e.row.path.clone()),
                    Some(x) => (x.clone(), r.path.clone()),
                };
                if !seen.insert(format!("{env}:{at}:{}", r.key)) || !caller.allows(Action::Read, &Scope::at(project, &env, &at)) {
                    continue;
                }
                let view = views.entry((env.clone(), at.clone())).or_insert_with(|| self.view(project, &env, &at, caller, false, Action::Read).unwrap_or_default());
                if let Some(t) = view.iter().find(|t| t.row.key == r.key) {
                    h.update(b"ref\0");
                    stamp(&mut h, t);
                    todo.push((t.clone(), depth + 1));
                }
            }
        }
        let have: Vec<String> = entries.iter().map(|e| e.row.key.clone()).collect();
        for m in self.missing_required(project, &path, &have)? {
            h.update(format!("required\0{}\n", m.key));
        }
        Ok(h.finalize()[..16].iter().map(|b| format!("{b:02x}")).collect())
    }

    pub fn versions(&self, project: &str, slug: &str, path: &str, key: &str, kind: Option<SecretType>, caller: &Caller) -> Result<Vec<Version>> {
        let path = names::path(path)?;
        caller.need(Action::Read, &Scope::at(project, slug, &path))?;
        self.project(project)?;
        let owner = self.owner_of(kind, caller);
        self.db.read(|c| {
            let env = envs::by_slug(c, project, slug)?;
            let v = c.query(
                "SELECT version, seq, op, what, fp, comment, tags, link, at, by, purged_at IS NOT NULL FROM versions WHERE env_id = ? AND path = ? AND key = ? AND owner = ? ORDER BY seq DESC LIMIT 500",
                [&env.id, &path, key, &owner],
                |r| {
                    Ok(Version {
                        version: r.get(0)?,
                        seq: r.get(1)?,
                        op: r.get(2)?,
                        what: r.get::<_, String>(3)?.split(',').map(Into::into).collect(),
                        fp: r.get(4)?,
                        comment: r.get(5)?,
                        tags: from_json(&r.get::<_, String>(6)?),
                        link: r.get::<_, Option<String>>(7)?.and_then(|s| serde_json::from_str(&s).ok()),
                        at: r.get(8)?,
                        by: r.get(9)?,
                        purged: r.get::<_, i64>(10)? != 0,
                    })
                },
            )?;
            if v.is_empty() {
                return Err(Error::NotFound(format!("no secret {key} in {slug}:{path}")));
            }
            Ok(v)
        })
    }

    /// An old version's stored value (unresolved). Audited.
    #[allow(clippy::too_many_arguments)]
    pub fn version_value(&self, project: &str, slug: &str, path: &str, key: &str, version: i64, kind: Option<SecretType>, caller: &Caller) -> Result<Value> {
        let path = names::path(path)?;
        caller.need(Action::Reveal, &Scope::at(project, slug, &path))?;
        let dek = self.project(project)?;
        let owner = self.owner_of(kind, caller);
        let (id, ct, link): (String, Option<Vec<u8>>, Option<String>) = self.db.read(|c| {
            let env = envs::by_slug(c, project, slug)?;
            c.query_row(
                "SELECT secret_id, value, link FROM versions WHERE env_id = ? AND path = ? AND key = ? AND owner = ? AND version = ?",
                params![env.id, path, key, owner, version],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(|_| Error::NotFound(format!("no version {version} of {key}")))
        })?;
        let v = match (ct, link) {
            (Some(ct), _) => dek.open(&value_aad(project, &id, version), &ct)?,
            (None, Some(l)) => {
                let l: Link = serde_json::from_str(&l).map_err(|_| Error::Internal("a stored link isn't JSON".into()))?;
                Value::new(format!("(linked: {} {} {})", l.kind, l.r#ref, l.field))
            }
            (None, None) => return Err(Error::NotFound(format!("version {version} of {key} is a deletion, or its value was purged"))),
        };
        self.audit_now(project, caller, "secrets.reveal", Some(slug), Some(&path), &[key.to_string()], &format!("revealed version {version} of {key}"));
        Ok(v)
    }

    fn owner_of(&self, kind: Option<SecretType>, caller: &Caller) -> String {
        match kind {
            Some(SecretType::Personal) => caller.principal.clone(),
            _ => String::new(),
        }
    }

    /// Keys as rows, standard environments as columns (Infisical's overview), with fingerprints.
    pub fn grid(&self, project: &str, path: &str, caller: &Caller) -> Result<Grid> {
        let path = names::path(path)?;
        caller.need(Action::Read, &Scope::project(project))?;
        let dek = self.project(project)?;
        let principal = caller.personal().then_some(caller.principal.as_str());
        let (envs, views, folders, previews) = self.db.read(|c| {
            let all = envs::all(c, project)?;
            let previews = all.iter().filter(|e| e.kind == Kind::Ephemeral).count() as i64;
            let envs: Vec<EnvRow> = all.into_iter().filter(|e| e.kind == Kind::Standard && caller.allows(Action::Read, &Scope::at(project, &e.slug, &path))).collect();
            let mut views = vec![];
            let mut folders = BTreeSet::new();
            for e in &envs {
                let through = |x: &str, p: &str| caller.allows(Action::Read, &Scope::at(project, x, p));
                views.push(view_in(c, e, &path, principal, &through, caller.grants.untrusted(), 0, &mut HashSet::new())?);
                for f in c.query("SELECT path FROM folders WHERE env_id = ?", [&e.id], |r| r.get::<_, String>(0))? {
                    if f != path && names::parents(&f).last() == Some(&path) {
                        folders.insert(f);
                    }
                }
            }
            Ok::<_, Error>((envs, views, folders, previews))
        })?;
        let mut keys: BTreeSet<String> = BTreeSet::new();
        for v in &views {
            keys.extend(v.iter().map(|e| e.row.key.clone()));
        }
        let all_required: Vec<crate::Required> = self.db.read(|c| crate::required::of(c, project))?.into_iter().filter(|r| r.path == path).collect();
        let required: BTreeSet<String> = all_required.iter().map(|r| r.key.clone()).collect();
        let typed: HashMap<&str, crate::required::Rule<'_>> = all_required.iter().filter(|r| r.kind.is_some() || r.pattern.is_some()).map(|r| (r.key.as_str(), r.rule())).collect();
        // Each environment's entries by key: a row looks each environment up once, not by scanning.
        let by_key: Vec<HashMap<&str, &Entry>> = views.iter().map(|v| v.iter().map(|e| (e.row.key.as_str(), e)).collect()).collect();
        keys.extend(required.iter().cloned());
        let rows = keys
            .into_iter()
            .map(|key| {
                let mut cells = BTreeMap::new();
                // The status compares real fingerprints; the cells show them only where the caller
                // may reveal.
                let mut fps: Vec<Option<&str>> = vec![];
                for (env, view) in envs.iter().zip(&by_key) {
                    let found = view.get(key.as_str()).copied();
                    fps.push(found.map(|e| e.row.fp.as_str()));
                    let cell = found.map(|e| {
                        // A row another project shares: its fingerprint is that project's, and its
                        // value its key's, so neither shows here.
                        let reveal = e.row.project_id == project && caller.allows(Action::Reveal, &Scope::at(project, &e.env, &e.row.path));
                        let refs = e.row.references(&dek, project);
                        let misfit = match (typed.get(key.as_str()), &e.row.value) {
                            (Some(r), Some(ct)) if reveal && e.row.link.is_none() && refs.is_empty() => {
                                dek.open(&value_aad(project, &e.row.id, e.row.version), ct).ok().and_then(|v| r.misfit(v.expose()))
                            }
                            _ => None,
                        };
                        Cell {
                            fp: if reveal { e.row.fp.clone() } else { String::new() },
                            version: e.row.version,
                            updated_at: e.row.updated_at,
                            link: e.row.link.clone(),
                            reference: !refs.is_empty(),
                            source: match e.source {
                                Source::Own => "own",
                                Source::Import { .. } => "import",
                                Source::Base { .. } => "base",
                            },
                            comment: !e.row.comment.is_empty(),
                            personal: !e.row.owner.is_empty(),
                            misfit,
                        }
                    });
                    cells.insert(env.slug.clone(), cell);
                }
                let present: Vec<&Cell> = cells.values().flatten().collect();
                let status = if present.len() < cells.len() {
                    "missing"
                } else if present.windows(2).all(|w| w[0].fp == w[1].fp) {
                    "same"
                } else {
                    "different"
                };
                GridRow { required: required.contains(&key), key, cells, status }
            })
            .collect();
        Ok(Grid { path, environments: envs.into_iter().map(|e| e.slug).collect(), rows, folders: folders.into_iter().collect(), previews })
    }

    /// Every key of two environments' folder with whether they match (fingerprints only).
    pub fn compare(&self, project: &str, from: &str, to: &str, path: &str, caller: &Caller) -> Result<serde_json::Value> {
        let path = names::path(path)?;
        caller.need(Action::Read, &Scope::at(project, from, &path))?;
        caller.need(Action::Read, &Scope::at(project, to, &path))?;
        self.project(project)?;
        let a = self.view(project, from, &path, caller, true, Action::Read)?;
        let b = self.view(project, to, &path, caller, true, Action::Read)?;
        let keys: BTreeSet<&str> = a.iter().chain(&b).map(|e| e.row.key.as_str()).collect();
        let (ia, ib): (HashMap<&str, &Entry>, HashMap<&str, &Entry>) = (a.iter().map(|e| (e.row.key.as_str(), e)).collect(), b.iter().map(|e| (e.row.key.as_str(), e)).collect());
        let rows: Vec<Json> = keys
            .into_iter()
            .map(|k| {
                let (x, y) = (ia.get(k).copied(), ib.get(k).copied());
                let status = match (x, y) {
                    (Some(x), Some(y)) if x.row.fp == y.row.fp => "same",
                    (Some(_), Some(_)) => "different",
                    (Some(_), None) => "only_from",
                    _ => "only_to",
                };
                serde_json::json!({ "key": k, "status": status, "from": x.map(|e| e.row.version), "to": y.map(|e| e.row.version) })
            })
            .collect();
        Ok(serde_json::json!({ "from": from, "to": to, "path": path, "rows": rows }))
    }

    /// Problems a write would leave: references to keys that don't exist, or to the key itself.
    pub(crate) fn warnings(&self, project: &str, slug: &str, items: &[Item], caller: &Caller) -> Vec<String> {
        let mut out = vec![];
        let mut views: HashMap<(String, String), Vec<String>> = HashMap::new();
        for item in items {
            let ItemAction::Set { value: Some(v), .. } = &item.action else { continue };
            for r in refs::refs(v.expose()) {
                let (env, path) = match &r.env {
                    None => (slug.to_string(), item.path.clone()),
                    Some(e) => (e.clone(), r.path.clone()),
                };
                if env == slug && path == item.path && r.key == item.key {
                    out.push(format!("{} references itself", item.key));
                    continue;
                }
                // Only where the caller may look: a warning must not tell whether a key exists in an
                // environment it can't read.
                if !caller.allows(Action::Read, &Scope::at(project, &env, &path)) {
                    continue;
                }
                let keys = views.entry((env.clone(), path.clone())).or_insert_with(|| {
                    let mut k: Vec<String> = self.view(project, &env, &path, caller, true, Action::Read).map(|v| v.into_iter().map(|e| e.row.key).collect()).unwrap_or_default();
                    if env == slug && path == item.path {
                        k.extend(items.iter().filter(|i| matches!(i.action, ItemAction::Set { .. })).map(|i| i.key.clone()));
                    }
                    k
                });
                if !keys.contains(&r.key) {
                    out.push(format!("{} references {}, which doesn't exist", item.key, r.display()));
                }
            }
        }
        out
    }
}

fn lease_note(leases: &[String]) -> String {
    match leases.len() {
        0 => String::new(),
        n => format!(" ({} issued)", crate::write::plural(n, "lease")),
    }
}

/// A dynamic credential issued during one resolve: its parts, its handle, its link.
type IssuedDynamic = (BTreeMap<String, Value>, Json, Link, (String, String));

/// Resolves values: decrypts, reads live links, issues dynamic ones (once per credential), and
/// follows references with cycle detection.
pub(crate) struct Resolver<'a> {
    s: &'a Secrets,
    project: &'a str,
    dek: &'a Dek,
    /// Sharing projects' data keys, by project.
    deks: HashMap<String, Dek>,
    caller: &'a Caller,
    resolve: bool,
    consumer: String,
    envs: HashMap<String, Option<EnvRow>>,
    views: HashMap<(String, String), HashMap<String, Entry>>,
    done: HashMap<String, (Value, Option<String>)>,
    stack: Vec<String>,
    /// The project's resources, loaded at the first link.
    members: Option<std::collections::BTreeSet<(String, String)>>,
    /// Dynamic credentials issued during this resolve, by (environment, kind, ref, credential).
    issued: BTreeMap<(String, String, String, String), std::result::Result<IssuedDynamic, String>>,
    /// Refuse dynamic links instead of issuing (syncs: a short-lived credential would expire at
    /// the target).
    pub no_dynamic: bool,
}

impl<'a> Resolver<'a> {
    pub fn new(s: &'a Secrets, project: &'a str, dek: &'a Dek, caller: &'a Caller, resolve: bool, consumer: &str) -> Self {
        Resolver { s, project, dek, caller, resolve, consumer: consumer.into(), envs: HashMap::new(), views: HashMap::new(), done: HashMap::new(), stack: vec![], members: None, issued: BTreeMap::new(), no_dynamic: false, deks: HashMap::new() }
    }

    fn env(&mut self, slug: &str) -> Option<EnvRow> {
        if !self.envs.contains_key(slug) {
            let e = self.s.db.read(|c| envs::by_slug(c, self.project, slug)).ok();
            self.envs.insert(slug.into(), e);
        }
        self.envs[slug].clone()
    }

    fn lookup(&mut self, env: &str, path: &str, key: &str) -> std::result::Result<Entry, String> {
        let k = (env.to_string(), path.to_string());
        if !self.views.contains_key(&k) {
            let v = self.s.view(self.project, env, path, self.caller, false, Action::Reveal).map_err(|e| e.to_string())?.into_iter().map(|e| (e.row.key.clone(), e)).collect();
            self.views.insert(k.clone(), v);
        }
        self.views[&k].get(key).cloned().ok_or_else(|| "doesn't exist".to_string())
    }

    /// An entry's value and the first problem resolving it (the value then keeps the
    /// unresolved reference's text).
    pub fn entry(&mut self, e: &Entry) -> (Value, Option<String>) {
        // By the environment's id, not its slug: a folder another project shares may have the same
        // slug and path as one of this project's.
        let id = format!("{}:{}:{}", e.row.env_id, e.row.path, e.row.key);
        if let Some(d) = self.done.get(&id) {
            return d.clone();
        }
        if let Some(at) = self.stack.iter().position(|s| *s == id) {
            let mut chain: Vec<String> = self.stack[at..].iter().map(|s| s.rsplit(':').next().unwrap_or_default().to_string()).collect();
            chain.push(e.row.key.clone());
            return (Value::default(), Some(format!("cycle: {}", chain.join(" → "))));
        }
        if self.stack.len() >= DEPTH {
            return (Value::default(), Some(format!("references are nested more than {DEPTH} deep")));
        }
        let raw = match self.raw(e) {
            Ok(v) => v,
            Err(p) => return (Value::default(), Some(p)),
        };
        let out = if self.resolve && e.row.link.as_ref().is_none_or(|l| l.mode == Mode::Managed) {
            self.stack.push(id.clone());
            let r = self.text(&e.env, &e.row.path, &raw);
            self.stack.pop();
            r
        } else {
            (raw, None)
        };
        self.done.insert(id, out.clone());
        out
    }

    fn raw(&mut self, e: &Entry) -> std::result::Result<Value, String> {
        if let Some(l) = e.row.link.as_ref().filter(|l| l.mode != Mode::Managed) {
            if self.members.is_none() {
                self.members = Some(self.s.members(self.project).map_err(|e| e.to_string())?);
            }
            if !self.members.as_ref().is_some_and(|m| m.contains(&(l.kind.clone(), l.r#ref.clone()))) {
                return Err(format!("{} {} isn't in this project any more", l.kind, l.r#ref));
            }
        }
        match &e.row.link {
            Some(l) if l.mode == Mode::Live => {
                let p = self.s.links.get(&l.kind).map_err(|e| e.to_string())?;
                p.read(&l.r#ref, &l.field, l).map_err(|err| format!("{} {}: {err}", l.kind, l.r#ref))
            }
            Some(l) if l.mode == Mode::Dynamic && self.no_dynamic => Err(format!("{} {} is dynamic: short-lived credentials aren't synced", l.kind, l.r#ref)),
            Some(l) if l.mode == Mode::Dynamic => {
                let cred = self.s.links.credential(l).unwrap_or("default").to_string();
                let group = (e.row.env_id.clone(), l.kind.clone(), l.r#ref.clone(), format!("{cred}:{}", json(&l.options)));
                if !self.issued.contains_key(&group) {
                    let label = format!("roll secrets {} {} for {}", e.env, e.row.key, self.consumer);
                    let r = self
                        .s
                        .links
                        .get(&l.kind)
                        .and_then(|p| p.issue(&l.r#ref, &cred, l, &label, Some(l.ttl_minutes())))
                        .map(|i| (i.parts, i.handle, l.clone(), (e.row.key.clone(), e.row.path.clone())))
                        .map_err(|err| format!("{} {}: {err}", l.kind, l.r#ref));
                    self.issued.insert(group.clone(), r);
                }
                match &self.issued[&group] {
                    Ok((parts, _, _, _)) => parts.get(&l.field).cloned().ok_or_else(|| format!("{} didn't issue {}", l.kind, l.field)),
                    Err(p) => Err(p.clone()),
                }
            }
            _ => match &e.row.value {
                Some(ct) => self.dek_of(&e.row.project_id).and_then(|dek| dek.open(&value_aad(&e.row.project_id, &e.row.id, e.row.version), ct).map_err(|_| "the stored value doesn't decrypt".to_string())),
                None => Ok(Value::default()),
            },
        }
    }

    /// The data key of `project`: this resolve's, or a sharing project's (cached).
    fn dek_of(&mut self, project: &str) -> std::result::Result<Dek, String> {
        if project == self.project {
            return Ok(self.dek.clone());
        }
        if let Some(d) = self.deks.get(project) {
            return Ok(d.clone());
        }
        let d = self.s.project(project).map_err(|_| format!("project {project}'s key isn't available"))?;
        self.deks.insert(project.to_string(), d.clone());
        Ok(d)
    }

    fn text(&mut self, env: &str, path: &str, raw: &Value) -> (Value, Option<String>) {
        let parts = refs::parse(raw.expose());
        if !parts.iter().any(|p| matches!(p, Part::Ref(..) | Part::Escaped)) {
            return (raw.clone(), None);
        }
        let mut out = zeroize::Zeroizing::new(String::new());
        let mut problem = None;
        for p in parts {
            match p {
                Part::Text(t) => out.push_str(t),
                Part::Escaped => out.push_str("${"),
                Part::Ref(r, text) => {
                    let (env, path) = match &r.env {
                        None => (env.to_string(), path.to_string()),
                        Some(e) => (e.clone(), r.path.clone()),
                    };
                    let got = if !self.caller.allows(Action::Reveal, &Scope::at(self.project, &env, &path)) {
                        Err(format!("{}: no access to {env}", r.display()))
                    } else if self.env(&env).is_none() {
                        Err(format!("{}: no environment {env}", r.display()))
                    } else {
                        self.lookup(&env, &path, &r.key).map_err(|p| format!("{} {p}", r.display())).and_then(|e| match self.entry(&e) {
                            (v, None) => Ok(v),
                            (_, Some(p)) if p.starts_with("cycle") => Err(p),
                            (_, Some(p)) => Err(format!("{}: {p}", r.display())),
                        })
                    };
                    match got {
                        Ok(v) => out.push_str(v.expose()),
                        Err(p) => {
                            out.push_str(text);
                            problem.get_or_insert(p);
                        }
                    }
                }
            }
        }
        (Value::new(out.as_str()), problem)
    }

    /// Record the leases of the dynamic credentials this resolve issued; revoke them if that
    /// fails. Returns their ids.
    pub fn finish(self, _slug: &str) -> Result<Vec<String>> {
        let issued: Vec<_> = self.issued.into_iter().filter_map(|((env_id, _, _, _), r)| r.ok().map(|(_, handle, link, at)| (env_id, handle, link, at))).collect();
        if issued.is_empty() {
            return Ok(vec![]);
        }
        let now = self.s.db.now_ms()?;
        let consumer = self.consumer.clone();
        let project = self.project.to_string();
        let ids: Vec<String> = issued.iter().map(|_| names::new_id("lse", 16)).collect();
        let res = self.s.db.write(|c| {
            for ((env_id, handle, link, (key, path)), id) in issued.iter().zip(&ids) {
                c.execute(
                    "INSERT INTO leases (id, project_id, env_id, key, path, kind, link, handle, consumer, issued_at, expires_at, revoked_at) VALUES (?, ?, ?, ?, ?, 'dynamic', ?, ?, ?, ?, ?, NULL)",
                    params![id, project, env_id, key, path, json(link), json(handle), consumer, now, now + link.ttl_minutes() as i64 * 60_000],
                )?;
            }
            Ok::<_, Error>(())
        });
        if let Err(e) = res {
            for (_, handle, link, _) in &issued {
                if let Ok(p) = self.s.links.get(&link.kind) {
                    let _ = p.revoke(&link.r#ref, handle);
                }
            }
            return Err(e);
        }
        Ok(ids)
    }
}

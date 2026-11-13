//! Environments (standard and ephemeral), their policies, folders and imports
//! (docs/secrets.md, "Environments", "Folders", "Imports").

use crate::access::{Action, Caller, Scope};
use crate::write::{Item, ItemAction};
use crate::{Error, Result, Secrets, audit, from_json, json, names, refs};
use code_sql::{Conn, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Standard,
    Ephemeral,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Standard => "standard",
            Kind::Ephemeral => "ephemeral",
        }
    }
}

/// An environment as stored.
#[derive(Clone, Debug)]
pub(crate) struct EnvRow {
    pub id: String,
    pub project_id: String,
    pub slug: String,
    pub name: String,
    pub position: i64,
    pub kind: Kind,
    pub base_id: Option<String>,
    pub labels: BTreeMap<String, String>,
    pub expires_at: Option<i64>,
    pub protected: bool,
    pub approvals: i64,
    pub approvers: Vec<String>,
    pub created_at: i64,
    /// Keys unchanged for longer are flagged stale (insights).
    pub max_age_days: Option<i64>,
    /// The host change requests are announced to (the URL itself is sealed).
    pub notify: Option<String>,
}

const ENV_COLUMNS: &str = "id, project_id, slug, name, position, kind, base_id, labels, expires_at, protected, approvals, approvers, created_at";
/// What reads select: the columns above, and the ones added since.
const ENV_SELECT: &str = "id, project_id, slug, name, position, kind, base_id, labels, expires_at, protected, approvals, approvers, created_at, max_age_days, notify_host";

fn env_row(r: &code_sql::Row<'_>) -> code_sql::Result<EnvRow> {
    let kind: String = r.get(5)?;
    Ok(EnvRow {
        id: r.get(0)?,
        project_id: r.get(1)?,
        slug: r.get(2)?,
        name: r.get(3)?,
        position: r.get(4)?,
        kind: if kind == "ephemeral" { Kind::Ephemeral } else { Kind::Standard },
        base_id: r.get(6)?,
        labels: from_json(&r.get::<_, String>(7)?),
        expires_at: r.get(8)?,
        protected: r.get::<_, i64>(9)? != 0,
        approvals: r.get(10)?,
        approvers: from_json(&r.get::<_, String>(11)?),
        created_at: r.get(12)?,
        max_age_days: r.get(13)?,
        notify: r.get(14)?,
    })
}

/// An environment as the API shows it.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Environment {
    pub id: String,
    pub slug: String,
    pub name: String,
    pub position: i64,
    pub kind: Kind,
    /// The base's slug (ephemeral environments).
    pub base: Option<String>,
    pub labels: BTreeMap<String, String>,
    pub expires_at: Option<i64>,
    pub protected: bool,
    pub approvals: i64,
    pub approvers: Vec<String>,
    pub created_at: i64,
    pub max_age_days: Option<i64>,
    /// The host opened change requests are announced to (`notify_url`'s; the URL stays sealed).
    pub notify: Option<String>,
    /// Shared secrets in every folder.
    pub secrets: i64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewEnvironment {
    pub slug: String,
    pub name: Option<String>,
    pub kind: Option<Kind>,
    pub base: Option<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    pub expires_at: Option<i64>,
    pub ttl_minutes: Option<i64>,
    /// An ephemeral environment's first overrides and links, written with it.
    #[serde(default)]
    pub secrets: Vec<crate::Op>,
    /// The folder `secrets` are written to (default `/`).
    pub path: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvChange {
    pub slug: Option<String>,
    pub name: Option<String>,
    /// `Some(None)`: no expiry.
    #[serde(default, deserialize_with = "some")]
    pub expires_at: Option<Option<i64>>,
    /// Flag keys unchanged for longer (insights); `Some(None)`: never.
    #[serde(default, deserialize_with = "some")]
    pub max_age_days: Option<Option<i64>>,
}

fn some<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(d: D) -> std::result::Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub protected: bool,
    #[serde(default)]
    pub approvals: Option<i64>,
    #[serde(default)]
    pub approvers: Option<Vec<String>>,
    /// Where opened change requests are announced (a Slack or Discord incoming webhook, any
    /// HTTPS URL); `null` stops it, absent keeps it. Kept sealed: the URL is a credential.
    #[serde(default, deserialize_with = "some")]
    pub notify_url: Option<Option<String>>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Import {
    pub environment: String,
    pub path: String,
}

#[derive(Debug, Serialize)]
pub struct Folder {
    pub path: String,
    pub name: String,
    pub secrets: i64,
    pub folders: i64,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn insert(
    c: &Conn<'_>,
    project: &str,
    slug: &str,
    name: &str,
    position: i64,
    kind: Kind,
    base_id: Option<&str>,
    labels: &BTreeMap<String, String>,
    expires_at: Option<i64>,
    now: i64,
) -> Result<EnvRow> {
    let id = names::new_id("env", 12);
    c.execute(
        &format!("INSERT INTO environments ({ENV_COLUMNS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 0, 0, '[]', ?)"),
        params![id, project, slug, name, position, kind.as_str(), base_id, json(labels), expires_at, now],
    )?;
    c.execute("INSERT INTO folders (env_id, path, created_at) VALUES (?, '/', ?) ON CONFLICT (env_id, path) DO NOTHING", params![id, now])?;
    Ok(EnvRow {
        id,
        project_id: project.into(),
        slug: slug.into(),
        name: name.into(),
        position,
        kind,
        base_id: base_id.map(Into::into),
        labels: labels.clone(),
        expires_at,
        protected: false,
        approvals: 0,
        approvers: vec![],
        created_at: now,
        max_age_days: None,
        notify: None,
    })
}

pub(crate) fn by_slug(c: &Conn<'_>, project: &str, slug: &str) -> Result<EnvRow> {
    c.query_row(&format!("SELECT {ENV_SELECT} FROM environments WHERE project_id = ? AND slug = ?"), [project, slug], env_row)
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("no environment {slug}")))
}

pub(crate) fn by_id(c: &Conn<'_>, id: &str) -> Result<EnvRow> {
    c.query_row(&format!("SELECT {ENV_SELECT} FROM environments WHERE id = ?"), [id], env_row)
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("no environment {id}")))
}

pub(crate) fn all(c: &Conn<'_>, project: &str) -> Result<Vec<EnvRow>> {
    Ok(c.query(&format!("SELECT {ENV_SELECT} FROM environments WHERE project_id = ? ORDER BY position, created_at"), [project], env_row)?)
}

fn view(c: &Conn<'_>, rows: Vec<EnvRow>) -> Result<Vec<Environment>> {
    let Some(first) = rows.first() else { return Ok(vec![]) };
    let counts: HashMap<String, i64> = c
        .query("SELECT env_id, COUNT(*) FROM secrets WHERE project_id = ? AND owner = '' GROUP BY env_id", [&first.project_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .into_iter()
        .collect();
    let slugs: HashMap<String, String> = all(c, &first.project_id)?.into_iter().map(|e| (e.id, e.slug)).collect();
    Ok(rows
        .into_iter()
        .map(|e| Environment {
            secrets: counts.get(&e.id).copied().unwrap_or(0),
            base: e.base_id.as_ref().and_then(|b| slugs.get(b).cloned()),
            id: e.id,
            slug: e.slug,
            name: e.name,
            position: e.position,
            kind: e.kind,
            labels: e.labels,
            expires_at: e.expires_at,
            protected: e.protected,
            approvals: e.approvals,
            approvers: e.approvers,
            created_at: e.created_at,
            max_age_days: e.max_age_days,
            notify: e.notify,
        })
        .collect())
}

fn one(c: &Conn<'_>, row: EnvRow) -> Result<Environment> {
    Ok(view(c, vec![row])?.remove(0))
}

impl Secrets {
    /// The project's environments the caller may read, in order.
    pub fn environments(&self, project: &str, caller: &Caller) -> Result<Vec<Environment>> {
        caller.need(Action::Read, &Scope::project(project))?;
        self.project(project)?;
        self.db.read(|c| {
            let rows = all(c, project)?.into_iter().filter(|e| caller.allows(Action::Read, &Scope::env(project, &e.slug))).collect();
            view(c, rows)
        })
    }

    /// `caller` as a consumer running code nobody vetted (a check, a sandbox, a preview): kept out
    /// of the project's protected environments, as they are when it asks.
    pub fn untrusted(&self, project: &str, caller: &Caller) -> Result<Caller> {
        self.project(project)?;
        let protected = self.db.read(|c| Ok::<_, Error>(all(c, project)?.into_iter().filter(|e| e.protected).map(|e| e.slug).collect()))?;
        Ok(caller.without(protected))
    }

    pub fn environment(&self, project: &str, slug: &str, caller: &Caller) -> Result<Environment> {
        caller.need(Action::Read, &Scope::env(project, slug))?;
        self.project(project)?;
        self.db.read(|c| {
            let e = by_slug(c, project, slug)?;
            one(c, e)
        })
    }

    pub fn create_environment(&self, project: &str, new: NewEnvironment, caller: &Caller) -> Result<Environment> {
        names::slug(&new.slug)?;
        caller.need(Action::Write, &Scope::env(project, &new.slug))?;
        let name = new.name.clone().unwrap_or_else(|| title(&new.slug));
        names::env_name(&name)?;
        names::labels(&new.labels)?;
        let kind = new.kind.unwrap_or(if new.base.is_some() { Kind::Ephemeral } else { Kind::Standard });
        if kind == Kind::Standard && (new.base.is_some() || new.expires_at.is_some() || new.ttl_minutes.is_some() || !new.secrets.is_empty()) {
            return Err(Error::BadRequest("only an ephemeral environment has a base, an expiry or secrets to create it with".into()));
        }
        if kind == Kind::Ephemeral && new.base.is_none() {
            return Err(Error::BadRequest("an ephemeral environment needs a base environment".into()));
        }
        let dek = self.project(project)?;
        let now = self.db.now_ms()?;
        let expires_at = match (new.expires_at, new.ttl_minutes) {
            (Some(at), _) => Some(at),
            (None, Some(m)) if m > 0 => Some(now + m * 60_000),
            (None, Some(_)) => return Err(Error::BadRequest("ttl_minutes is positive".into())),
            (None, None) => None,
        };
        let path = names::path(new.path.as_deref().unwrap_or("/"))?;
        // An ephemeral environment shows its base's values: whoever makes one must be able to
        // read them.
        if let Some(b) = &new.base {
            caller.need(Action::Reveal, &Scope::at(project, b, &path))?;
        }
        let items = crate::write::items(&path, &new.secrets, caller)?;
        self.check_items(project, &new.slug, &items, caller)?;
        self.check_types(project, &items)?;
        let items = self.prepare(project, &new.slug, &items, caller)?;
        let env = self.db.write(|c| {
            if by_slug(c, project, &new.slug).is_ok() {
                return Err(Error::Conflict("conflict", format!("an environment named {} exists", new.slug)));
            }
            let base = match &new.base {
                Some(b) => {
                    let b = by_slug(c, project, b)?;
                    if b.kind != Kind::Standard {
                        return Err(Error::BadRequest("a base is a standard environment".into()));
                    }
                    Some(b.id)
                }
                None => None,
            };
            let position = match kind {
                Kind::Standard => c.query_row("SELECT COALESCE(MAX(position), -1) + 1 FROM environments WHERE project_id = ? AND kind = 'standard'", [project], |r| r.get(0))?,
                Kind::Ephemeral => 1_000_000,
            };
            let env = insert(c, project, &new.slug, &name, position, kind, base.as_deref(), &new.labels, expires_at, now)?;
            audit::record(c, project, caller, "environment.create", Some(&env.slug), None, &[], &format!("created {} environment {}", kind.as_str(), env.slug), now)?;
            if !items.is_empty() {
                crate::write::apply(c, &dek, &env, &items.items, "write", "created with the environment", caller, now)?;
            }
            one(c, env)
        });
        if env.is_err() {
            self.discard(&items);
        }
        env
    }

    pub fn update_environment(&self, project: &str, slug: &str, change: EnvChange, caller: &Caller) -> Result<Environment> {
        caller.need(Action::Admin, &Scope::env(project, slug))?;
        if let Some(s) = &change.slug {
            names::slug(s)?;
            caller.need(Action::Admin, &Scope::env(project, s))?;
        }
        if let Some(n) = &change.name {
            names::env_name(n)?;
        }
        let dek = self.project(project)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let mut env = by_slug(c, project, slug)?;
            if let Some(n) = &change.name {
                c.execute("UPDATE environments SET name = ? WHERE id = ?", [n, &env.id])?;
                env.name = n.clone();
            }
            if let Some(at) = change.expires_at {
                if env.kind != Kind::Ephemeral {
                    return Err(Error::BadRequest("only an ephemeral environment expires".into()));
                }
                c.execute("UPDATE environments SET expires_at = ? WHERE id = ?", params![at, env.id])?;
                env.expires_at = at;
            }
            if let Some(d) = change.max_age_days {
                if d.is_some_and(|d| !(1..=3650).contains(&d)) {
                    return Err(Error::BadRequest("max_age_days is 1–3650".into()));
                }
                c.execute("UPDATE environments SET max_age_days = ? WHERE id = ?", params![d, env.id])?;
                env.max_age_days = d;
            }
            if let Some(new) = change.slug.as_deref().filter(|s| *s != slug) {
                if by_slug(c, project, new).is_ok() {
                    return Err(Error::Conflict("conflict", format!("an environment named {new} exists")));
                }
                c.execute("UPDATE environments SET slug = ? WHERE id = ?", [new, &env.id])?;
                env.slug = new.to_string();
                rewrite_references(c, &dek, project, slug, new, caller, now)?;
                audit::record(c, project, caller, "environment.rename", Some(new), None, &[], &format!("renamed {slug} to {new}"), now)?;
            }
            one(c, env)
        })
    }

    pub fn order_environments(&self, project: &str, order: &[String], caller: &Caller) -> Result<Vec<Environment>> {
        caller.need(Action::Admin, &Scope::project(project))?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let envs: Vec<EnvRow> = all(c, project)?.into_iter().filter(|e| e.kind == Kind::Standard).collect();
            let mut sorted: Vec<&str> = order.iter().map(String::as_str).collect();
            sorted.sort();
            let mut have: Vec<&str> = envs.iter().map(|e| e.slug.as_str()).collect();
            have.sort();
            if sorted != have {
                return Err(Error::BadRequest(format!("order lists every standard environment once: {}", have.join(", "))));
            }
            for (i, slug) in order.iter().enumerate() {
                c.execute("UPDATE environments SET position = ? WHERE project_id = ? AND slug = ?", params![i as i64, project, slug])?;
            }
            audit::record(c, project, caller, "environment.order", None, None, &[], &order.join(" → "), now)?;
            let rows = all(c, project)?;
            view(c, rows)
        })
    }

    pub fn set_policy(&self, project: &str, slug: &str, p: Policy, caller: &Caller) -> Result<Environment> {
        caller.need(Action::Admin, &Scope::env(project, slug))?;
        let approvals = p.approvals.unwrap_or(if p.protected { 1 } else { 0 });
        if !(0..=10).contains(&approvals) {
            return Err(Error::BadRequest("approvals is 0–10".into()));
        }
        let approvers = p.approvers.unwrap_or_default();
        if approvers.len() > 50 || approvers.iter().any(|a| a.is_empty() || a.len() > 200) {
            return Err(Error::BadRequest("at most 50 approvers, each a principal".into()));
        }
        if let Some(Some(u)) = &p.notify_url {
            self.deliver.as_ref().ok_or_else(|| Error::BadRequest("notifications aren't on this server".into()))?.check(u)?;
        }
        let dek = self.project(project)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let env = by_slug(c, project, slug)?;
            match &p.notify_url {
                Some(Some(u)) => {
                    let sealed = dek.seal(&crate::notices::notify_aad(project, &env.id), &crate::Value::new(u.trim()));
                    let host = crate::names::url_host(u);
                    c.execute("UPDATE environments SET notify_url = ?, notify_host = ? WHERE id = ?", params![sealed, host, env.id])?;
                }
                Some(None) => {
                    c.execute("UPDATE environments SET notify_url = NULL, notify_host = NULL WHERE id = ?", [&env.id])?;
                }
                None => {}
            }
            if env.kind == Kind::Ephemeral && p.protected {
                return Err(Error::BadRequest("an ephemeral environment can't be protected".into()));
            }
            c.execute("UPDATE environments SET protected = ?, approvals = ?, approvers = ? WHERE id = ?", params![p.protected as i64, approvals, json(&approvers), env.id])?;
            let summary = if p.protected { format!("protected, {approvals} approval{}", if approvals == 1 { "" } else { "s" }) } else { "unprotected".into() };
            audit::record(c, project, caller, "environment.policy", Some(slug), None, &[], &summary, now)?;
            let env = by_id(c, &env.id)?;
            one(c, env)
        })
    }

    /// Delete an environment and everything in it, revoking its leases first.
    pub fn delete_environment(&self, project: &str, slug: &str, confirm: Option<&str>, caller: &Caller) -> Result<()> {
        caller.need(Action::Delete, &Scope::env(project, slug))?;
        self.project(project)?;
        let env = self.db.read(|c| by_slug(c, project, slug))?;
        if env.protected && confirm != Some(slug) {
            return Err(Error::BadRequest(format!("{slug} is protected: pass ?confirm={slug} to delete it")));
        }
        self.revoke_leases_where("env_id = ?", &env.id);
        let managed = self.managed_where("env_id = ?", &env.id);
        let now = self.db.now_ms()?;
        let done = self.db.write(|c| {
            let env = by_slug(c, project, slug)?;
            if env.kind == Kind::Standard {
                let standard: i64 = c.query_row("SELECT COUNT(*) FROM environments WHERE project_id = ? AND kind = 'standard'", [project], |r| r.get(0))?;
                if standard <= 1 {
                    return Err(Error::Conflict("last_environment", "a project keeps at least one environment".into()));
                }
                let dependants: i64 = c.query_row("SELECT COUNT(*) FROM environments WHERE base_id = ?", [&env.id], |r| r.get(0))?;
                if dependants > 0 {
                    return Err(Error::Conflict("conflict", format!("{dependants} ephemeral environment(s) are based on {slug}: delete them first")));
                }
            }
            drop_env(c, &env)?;
            audit::record(c, project, caller, "environment.delete", Some(slug), None, &[], &format!("deleted environment {slug}"), now)?;
            Ok(())
        });
        if done.is_ok() {
            self.revoke_managed(managed);
        }
        done
    }

    /// Delete every ephemeral environment with label `k=v`. Returns their slugs.
    pub fn delete_by_label(&self, project: &str, label: &str, caller: &Caller) -> Result<Vec<String>> {
        let (k, v) = label.split_once('=').ok_or_else(|| Error::BadRequest("label is key=value".into()))?;
        caller.need(Action::Read, &Scope::project(project))?;
        self.project(project)?;
        let doomed: Vec<EnvRow> = self.db.read(|c| Ok::<_, Error>(all(c, project)?.into_iter().filter(|e| e.kind == Kind::Ephemeral && e.labels.get(k).is_some_and(|x| x == v)).collect()))?;
        for e in &doomed {
            caller.need(Action::Delete, &Scope::env(project, &e.slug))?;
        }
        let mut deleted = vec![];
        for e in doomed {
            self.revoke_leases_where("env_id = ?", &e.id);
            let managed = self.managed_where("env_id = ?", &e.id);
            let now = self.db.now_ms()?;
            self.db.write(|c| {
                drop_env(c, &e)?;
                audit::record(c, project, caller, "environment.delete", Some(&e.slug), None, &[], &format!("deleted ephemeral environment {} ({k}={v})", e.slug), now)
            })?;
            self.revoke_managed(managed);
            deleted.push(e.slug);
        }
        Ok(deleted)
    }

    pub fn folders(&self, project: &str, slug: &str, path: &str, caller: &Caller) -> Result<Vec<Folder>> {
        let path = names::path(path)?;
        caller.need(Action::Read, &Scope::at(project, slug, &path))?;
        self.project(project)?;
        self.db.read(|c| {
            let env = by_slug(c, project, slug)?;
            let all_paths = c.query("SELECT path FROM folders WHERE env_id = ?", [&env.id], |r| r.get::<_, String>(0))?;
            let counts: HashMap<String, i64> =
                c.query("SELECT path, COUNT(*) FROM secrets WHERE env_id = ? AND owner = '' GROUP BY path", [&env.id], |r| Ok((r.get(0)?, r.get(1)?)))?.into_iter().collect();
            let mut out: Vec<Folder> = all_paths
                .iter()
                .filter(|p| p.as_str() != path && names::parents(p).last().map(String::as_str) == Some(path.as_str()))
                .map(|p| Folder {
                    path: p.clone(),
                    name: p.rsplit('/').next().unwrap_or_default().to_string(),
                    secrets: counts.get(p).copied().unwrap_or(0),
                    folders: all_paths.iter().filter(|q| names::parents(q).last() == Some(p)).count() as i64,
                })
                .filter(|f| caller.allows(Action::Read, &Scope::at(project, slug, &f.path)))
                .collect();
            out.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(out)
        })
    }

    pub fn create_folder(&self, project: &str, slug: &str, path: &str, caller: &Caller) -> Result<Folder> {
        let path = names::path(path)?;
        if path == "/" {
            return Err(Error::BadRequest("/ always exists".into()));
        }
        caller.need(Action::Write, &Scope::at(project, slug, &path))?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let env = by_slug(c, project, slug)?;
            if c.query_row("SELECT 1 FROM folders WHERE env_id = ? AND path = ?", [&env.id, &path], |_| Ok(())).optional()?.is_some() {
                return Err(Error::Conflict("conflict", format!("{path} exists")));
            }
            ensure_folders(c, &env.id, &path, now)?;
            audit::record(c, project, caller, "folder.create", Some(slug), Some(&path), &[], &format!("created {path}"), now)?;
            Ok(Folder { name: path.rsplit('/').next().unwrap_or_default().into(), path: path.clone(), secrets: 0, folders: 0 })
        })
    }

    /// Delete a folder and what's under it, as one change.
    pub fn delete_folder(&self, project: &str, slug: &str, path: &str, caller: &Caller) -> Result<Option<crate::write::Change>> {
        let path = names::path(path)?;
        if path == "/" {
            return Err(Error::BadRequest("/ can't be deleted".into()));
        }
        caller.need(Action::Delete, &Scope::at(project, slug, &path))?;
        let dek = self.project(project)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let env = by_slug(c, project, slug)?;
            if env.protected {
                return Err(Error::Conflict("approval_required", format!("{slug} is protected: deleting a folder needs a change request")));
            }
            let under = |p: &str| names::under(p, &path);
            let rows = c.query("SELECT path, key, owner FROM secrets WHERE env_id = ?", [&env.id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
            let items: Vec<Item> =
                rows.into_iter().filter(|(p, _, _)| under(p)).map(|(path, key, owner)| Item { path, key, owner, action: ItemAction::Delete }).collect();
            let folders = c.query("SELECT path FROM folders WHERE env_id = ?", [&env.id], |r| r.get::<_, String>(0))?;
            if !folders.contains(&path) {
                return Err(Error::NotFound(format!("no folder {path}")));
            }
            let change = if items.is_empty() { None } else { Some(crate::write::apply(c, &dek, &env, &items, "write", &format!("deleted folder {path}"), caller, now)?.0) };
            for f in folders.iter().filter(|f| under(f)) {
                c.execute("DELETE FROM folders WHERE env_id = ? AND path = ?", [&env.id, f])?;
                c.execute("DELETE FROM imports WHERE env_id = ? AND path = ?", [&env.id, f])?;
            }
            audit::record(c, project, caller, "folder.delete", Some(slug), Some(&path), &[], &format!("deleted {path}"), now)?;
            Ok(change)
        })
    }

    pub fn imports(&self, project: &str, slug: &str, path: &str, caller: &Caller) -> Result<Vec<Import>> {
        let path = names::path(path)?;
        caller.need(Action::Read, &Scope::at(project, slug, &path))?;
        self.project(project)?;
        self.db.read(|c| {
            let env = by_slug(c, project, slug)?;
            imports_of(c, &env.id, &path)
        })
    }

    pub fn set_imports(&self, project: &str, slug: &str, path: &str, list: Vec<Import>, caller: &Caller) -> Result<Vec<Import>> {
        let path = names::path(path)?;
        caller.need(Action::Write, &Scope::at(project, slug, &path))?;
        if list.len() > 20 {
            return Err(Error::BadRequest("at most 20 imports per folder".into()));
        }
        let mut list = list;
        for i in &mut list {
            i.path = names::path(&i.path)?;
            caller.need(Action::Reveal, &Scope::at(project, &i.environment, &i.path))?;
            if i.environment == slug && i.path == path {
                return Err(Error::BadRequest("a folder can't import itself".into()));
            }
        }
        self.project(project)?;
        let now = self.db.now_ms()?;
        // On a protected environment, imports change what it resolves to without a change: an
        // admin's decision, like its policy.
        if self.db.read(|c| by_slug(c, project, slug))?.protected {
            caller.need(Action::Admin, &Scope::at(project, slug, &path))?;
        }
        self.db.write(|c| {
            let env = by_slug(c, project, slug)?;
            c.execute("DELETE FROM imports WHERE env_id = ? AND path = ?", [&env.id, &path])?;
            for (n, i) in list.iter().enumerate() {
                let from = by_slug(c, project, &i.environment)?;
                c.execute(
                    "INSERT INTO imports (env_id, path, position, from_env_id, from_path) VALUES (?, ?, ?, ?, ?)",
                    params![env.id, path, n as i64, from.id, i.path],
                )?;
            }
            ensure_folders(c, &env.id, &path, now)?;
            let summary = list.iter().map(|i| format!("{}:{}", i.environment, i.path)).collect::<Vec<_>>().join(", ");
            audit::record(c, project, caller, "imports.set", Some(slug), Some(&path), &[], &format!("imports: {}", if summary.is_empty() { "none" } else { &summary }), now)?;
            imports_of(c, &env.id, &path)
        })
    }
}

pub(crate) fn imports_of(c: &Conn<'_>, env_id: &str, path: &str) -> Result<Vec<Import>> {
    Ok(c.query(
        "SELECT e.slug, i.from_path FROM imports i JOIN environments e ON e.id = i.from_env_id WHERE i.env_id = ? AND i.path = ? ORDER BY i.position",
        [env_id, path],
        |r| Ok(Import { environment: r.get(0)?, path: r.get(1)? }),
    )?)
}

/// `path` and its parents exist as folders.
pub(crate) fn ensure_folders(c: &Conn<'_>, env_id: &str, path: &str, now: i64) -> Result<()> {
    for p in names::parents(path).iter().map(String::as_str).chain([path]) {
        c.execute("INSERT INTO folders (env_id, path, created_at) VALUES (?, ?, ?) ON CONFLICT (env_id, path) DO NOTHING", params![env_id, p, now])?;
    }
    Ok(())
}

fn drop_env(c: &Conn<'_>, env: &EnvRow) -> Result<()> {
    for t in ["secrets", "versions", "requests", "leases", "folders", "imports"] {
        c.execute(&format!("DELETE FROM {t} WHERE env_id = ?"), [&env.id])?;
    }
    c.execute("DELETE FROM imports WHERE from_env_id = ?", [&env.id])?;
    c.execute("DELETE FROM changes WHERE env_id = ?", [&env.id])?;
    c.execute("DELETE FROM environments WHERE id = ?", [&env.id])?;
    Ok(())
}

/// After a rename from `from` to `to`: every value in the project that references `from` points
/// at `to`, as one change per environment that had one.
fn rewrite_references(c: &Conn<'_>, dek: &crate::crypto::Dek, project: &str, from: &str, to: &str, caller: &Caller, now: i64) -> Result<()> {
    let rows = c.query(
        "SELECT id, env_id, path, key, owner, version, value FROM secrets WHERE project_id = ? AND value IS NOT NULL",
        [project],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, i64>(5)?, r.get::<_, Vec<u8>>(6)?)),
    )?;
    let mut by_env: BTreeMap<String, Vec<Item>> = BTreeMap::new();
    for (id, env_id, path, key, owner, version, ct) in rows {
        let v = dek.open(&crate::crypto::value_aad(project, &id, version), &ct)?;
        if let Some(new) = refs::rename_env(v.expose(), from, to) {
            by_env.entry(env_id).or_default().push(Item { path, key, owner, action: ItemAction::set_value(crate::Value::new(new)) });
        }
    }
    for (env_id, items) in by_env {
        let env = by_id(c, &env_id)?;
        crate::write::apply(c, dek, &env, &items, "rename", &format!("references to {from} now point at {to}"), caller, now)?;
    }
    Ok(())
}

/// `development` → `Development`, `eu-prod` → `Eu prod`.
fn title(slug: &str) -> String {
    let s = slug.replace('-', " ");
    let mut cs = s.chars();
    cs.next().map(|f| f.to_uppercase().collect::<String>() + cs.as_str()).unwrap_or_default()
}

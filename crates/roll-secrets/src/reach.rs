//! Reach (docs/secrets.md, "Reach"): every way a folder's values (or one key's) can leave roll,
//! for a security review or before rotating after a leak. Who could (tokens and workload
//! identities whose scope covers it, untrusted code when the environment isn't protected), where it
//! goes by itself (syncs, other projects it's shared with, folders that import it), and who
//! actually read it lately (the audit log). Names and metadata only; never a value.

use crate::access::{Action, Caller, Grants, Scope, TokenScope};
use crate::{Error, Result, Secrets, envs, from_json, names};
use code_sql::params;
use serde::Serialize;
use std::collections::BTreeMap;

/// How far back "read lately" looks.
pub const READ_DAYS: i64 = 30;
/// The audit rows read for it, newest first.
const READS_MAX: i64 = 20_000;

#[derive(Debug, Serialize)]
pub struct Reach {
    pub environment: String,
    pub path: String,
    pub key: Option<String>,
    pub protected: bool,
    /// Checks, sandboxes and previews running code roll hasn't vetted may read it: the
    /// environment isn't protected (they never read a protected one).
    pub untrusted: bool,
    pub tokens: Vec<Holder>,
    pub identities: Vec<Holder>,
    pub syncs: Vec<SyncReach>,
    /// Other projects this folder is shared with (their importing folders read it).
    pub projects: Vec<String>,
    /// This project's folders that import it (`env:/path`): their readers read it too.
    pub imported_by: Vec<String>,
    /// Who read its values in the last [`READ_DAYS`] days, most recent first.
    pub readers: Vec<Reader>,
}

/// A token or identity that reaches the folder, with what it may do there.
#[derive(Debug, Serialize)]
pub struct Holder {
    pub id: String,
    pub name: String,
    /// Of read, reveal, write and admin: what it may do here.
    pub actions: Vec<Action>,
    pub expires_at: Option<i64>,
    pub last_used_at: Option<i64>,
    pub created_by: String,
}

#[derive(Debug, Serialize)]
pub struct SyncReach {
    pub id: String,
    pub kind: String,
    pub target: serde_json::Value,
    pub last_synced_at: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct Reader {
    pub actor: String,
    pub last_at: i64,
    pub reads: usize,
}

const ACTIONS: [Action; 4] = [Action::Read, Action::Reveal, Action::Write, Action::Admin];

/// What a scope (a token's, an identity's) may do at `slug:path`.
fn here(project: &str, environments: &Option<Vec<String>>, paths: &Option<Vec<String>>, actions: &[Action], slug: &str, path: &str) -> Vec<Action> {
    let scope = TokenScope { project: project.to_string(), environments: environments.clone(), paths: paths.clone(), actions: actions.to_vec() };
    ACTIONS.into_iter().filter(|a| scope.allows(*a, &Scope::at(project, slug, path))).collect()
}

impl Secrets {
    /// Reach of `slug:path` (one key's, with `key`). A project admin's: it lists the project's
    /// credentials, as the tokens list does.
    pub fn reach(&self, project: &str, slug: &str, path: &str, key: Option<&str>, caller: &Caller) -> Result<Reach> {
        caller.need(Action::Admin, &Scope::project(project))?;
        let path = names::path(path)?;
        let now = self.db.now_ms()?;
        let tokens = self
            .tokens(project, caller)?
            .into_iter()
            .filter(|t| t.expires_at.is_none_or(|e| e > now))
            .filter_map(|t| {
                let actions = here(project, &t.environments, &t.paths, &t.actions, slug, &path);
                (!actions.is_empty()).then_some(Holder { id: t.id, name: t.name, actions, expires_at: t.expires_at, last_used_at: t.last_used_at, created_by: t.created_by })
            })
            .collect();
        let identities = self
            .identities(project, caller)?
            .into_iter()
            .filter_map(|i| {
                let actions = here(project, &i.environments, &i.paths, &i.actions, slug, &path);
                (!actions.is_empty()).then_some(Holder { id: i.id, name: format!("{} ({} {})", i.name, i.issuer, i.subject), actions, expires_at: None, last_used_at: i.last_used_at, created_by: i.created_by })
            })
            .collect();
        let syncs = self
            .list_syncs(project, caller)?
            .into_iter()
            .filter(|s| s.environment == slug && s.path == path && key.is_none_or(|k| s.keys.as_ref().is_none_or(|ks| ks.iter().any(|x| x == k))))
            .map(|s| SyncReach { id: s.id, kind: s.kind, target: s.target, last_synced_at: s.last_synced_at })
            .collect();
        let projects = self.exports(project, slug, &path, caller)?.into_iter().map(|e| e.project).collect();
        let (env, imported_by, reads) = self.db.read(|c| {
            let env = envs::by_slug(c, project, slug)?;
            let imported_by: Vec<String> = c.query(
                "SELECT e.slug, i.path FROM imports i JOIN environments e ON e.id = i.env_id WHERE i.from_env_id = ? AND i.from_path = ? AND e.project_id = ? ORDER BY e.slug, i.path",
                params![env.id, path, project],
                |r| Ok(format!("{}:{}", r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )?;
            let reads: Vec<(String, i64, String)> = c.query(
                "SELECT actor, at, keys FROM audit WHERE project_id = ? AND env = ? AND path = ? AND at >= ? AND action IN ('secrets.reveal', 'secrets.export', 'secrets.resolve', 'sync.push') ORDER BY id DESC LIMIT ?",
                params![project, slug, path, now - READ_DAYS * 86_400_000, READS_MAX],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            Ok::<_, Error>((env, imported_by, reads))
        })?;
        let mut readers: BTreeMap<String, Reader> = BTreeMap::new();
        for (actor, at, keys) in reads {
            if key.is_some_and(|k| !from_json::<Vec<String>>(&keys).iter().any(|x| x == k)) {
                continue;
            }
            let r = readers.entry(actor.clone()).or_insert(Reader { actor, last_at: at, reads: 0 });
            r.last_at = r.last_at.max(at);
            r.reads += 1;
        }
        let mut readers: Vec<Reader> = readers.into_values().collect();
        readers.sort_by(|a, b| b.last_at.cmp(&a.last_at));
        Ok(Reach { environment: slug.to_string(), path, key: key.map(String::from), protected: env.protected, untrusted: !env.protected, tokens, identities, syncs, projects, imported_by, readers })
    }
}

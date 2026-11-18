//! Reach (docs/secrets.md, "Reach"): every way a folder's values (or one key's) can leave roll,
//! for a security review or before rotating after a leak. Who could (tokens and workload
//! identities whose scope covers it, untrusted code when the environment isn't protected), where it
//! goes by itself (syncs, other projects it's shared with, folders that import it), and who
//! actually read it lately (the audit log). Names and metadata only; never a value.

use crate::access::{Action, Caller, GrantGrants, Grants, Scope};
use crate::{Error, Result, Secrets, envs, from_json, names};
use code_sql::params;
use serde::Serialize;
use std::collections::BTreeMap;

/// How far back "read lately" looks.
pub const READ_DAYS: i64 = 30;
/// The audit rows read per project, newest first (a project's reads in [`READ_DAYS`]).
const READS_MAX: i64 = 100_000;

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
    /// Live dynamic credentials issued for its keys (or the key): their holders keep them until
    /// they end.
    pub leases: Vec<LeaseReach>,
    /// Open one-time shares made from its keys (or the key), as their makers said (`from`).
    pub shares: Vec<ShareReach>,
    /// Open one-time shares of the project that don't say where their value came from: they may
    /// hold one of its values too (only their makers know).
    pub unattributed_shares: usize,
    /// Other projects this folder is shared with (their importing folders read it).
    pub projects: Vec<String>,
    /// Folders that import it (`env:/path`) and environments based on it, and theirs; another
    /// project's only as `project prj_…`. Their readers read it too, and are counted below.
    pub imported_by: Vec<String>,
    /// The walk stopped at [`SOURCES_MAX`] folders: there are more.
    pub truncated: bool,
    /// Who read its values in the last [`READ_DAYS`] days, most recent first: here, through a folder
    /// that imports it, or an environment based on it. Reads through a `${…}` or `roll://` reference
    /// in another key are recorded where they were read, and aren't counted ([`UNCOUNTED`]).
    pub readers: Vec<Reader>,
    pub uncounted: &'static str,
    /// The folder is frozen (`…/reach/cut-off` with `freeze`): only its admins read it.
    pub frozen: Option<crate::cutoff::Freeze>,
}

#[derive(Debug, Serialize)]
pub struct LeaseReach {
    pub id: String,
    pub key: String,
    pub consumer: String,
    pub issued_at: i64,
    pub expires_at: i64,
}

#[derive(Debug, Serialize)]
pub struct ShareReach {
    pub id: String,
    pub note: String,
    pub key: String,
    pub created_by: String,
    pub expires_at: i64,
}

/// How many steps away a reading folder is followed (an importer's importer is two).
const SOURCE_DEPTH: usize = 8;
/// The most reading folders followed: each costs a few queries.
const SOURCES_MAX: usize = 256;

/// What `readers` doesn't count.
pub const UNCOUNTED: &str = "reads through a ${…} or roll:// reference in another key or file";

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
    /// An identity an incident's cut-off disabled: it doesn't log in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_at: Option<i64>,
    /// Why this caller's cut-off would leave it: a token that reaches beyond this project is only
    /// the operator's to revoke ([`BEYOND`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<&'static str>,
}

/// Why a cut-off leaves a token to the operator: its rules reach past this project
/// ([`crate::access::confined_to`]), so revoking it would cut off other projects too.
pub const BEYOND: &str = "reaches beyond this project: the operator revokes it";

#[derive(Debug, Serialize)]
pub struct SyncReach {
    pub id: String,
    pub kind: String,
    pub target: serde_json::Value,
    pub last_synced_at: Option<i64>,
    /// Paused by an incident's cut-off: it pushes nothing.
    pub paused_at: Option<i64>,
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
    // As roll-access would judge the same rule (an identity's login gets exactly this one).
    let g = roll_auth::Grant { id: "".into(), name: "".into(), kind: roll_auth::GrantKind::Token, scope: std::sync::Arc::new(roll_auth::scope::Scope(vec![crate::access::rule(project, environments.clone(), paths.clone(), actions)])), user: None, ask_otherwise: false };
    granted(&g, project, slug, path)
}

/// What grant `g` may do at `slug:path`, as roll secrets checks each request of it.
fn granted(g: &roll_auth::Grant, project: &str, slug: &str, path: &str) -> Vec<Action> {
    let g = GrantGrants(g.clone());
    ACTIONS.into_iter().filter(|a| g.allows(*a, &Scope::at(project, slug, path))).collect()
}

impl Secrets {
    /// Reach of `slug:path` (one key's, with `key`). A project admin's: it lists the project's
    /// credentials, as the tokens list does.
    pub fn reach(&self, project: &str, slug: &str, path: &str, key: Option<&str>, caller: &Caller) -> Result<Reach> {
        caller.need(Action::Admin, &Scope::project(project))?;
        let path = names::path(path)?;
        let now = self.db.now_ms()?;
        // roll-access's live tokens (not workload logins: those are their identities, below).
        let all = match &self.workloads {
            Some(w) => w.grants()?,
            None => vec![],
        };
        // A project's admin cuts off only what stays inside the project; the operator, anything.
        let operator = caller.grants.everything();
        let tokens = all
            .into_iter()
            .filter_map(|g| {
                // As each of its requests is judged here: a person's own token only within their own
                // projects (a token that reaches nothing here isn't listed, nor its name or person).
                let grants = self.grant_grants(&g).ok()?;
                let actions: Vec<Action> = ACTIONS.into_iter().filter(|a| grants.allows(*a, &Scope::at(project, slug, &path))).collect();
                let skipped = (!operator && !crate::access::confined_to(&g, project)).then_some(BEYOND);
                (!actions.is_empty()).then(|| Holder { id: g.id.to_string(), name: g.name.to_string(), actions, expires_at: None, last_used_at: None, created_by: g.user.as_deref().unwrap_or("operator").to_string(), disabled_at: None, skipped })
            })
            .collect();
        let identities = self
            .identities(project, caller)?
            .into_iter()
            // The project's own rows, and their logins carry one rule naming this project
            // (`access::rule`): always confined, so any of its admins cuts them off.
            .filter_map(|i| {
                let actions = here(project, &i.environments, &i.paths, &i.actions, slug, &path);
                (!actions.is_empty()).then_some(Holder { id: i.id, name: format!("{} ({} {})", i.name, i.issuer, i.subject), actions, expires_at: None, last_used_at: i.last_used_at, created_by: i.created_by, disabled_at: i.disabled_at, skipped: None })
            })
            .collect();
        let projects = self.exports(project, slug, &path, caller)?.into_iter().map(|e| e.project).collect();
        // Where its values are read besides here: folders that import it (in any project it's
        // shared with) and environments based on it (previews). Reads are recorded where they
        // happen, so each of those is looked at too.
        type Source = (String, String, String);
        type Held = (Vec<LeaseReach>, Vec<ShareReach>, usize, Option<crate::cutoff::Freeze>);
        let (env, sources, reads, truncated, (leases, shares, unattributed_shares, frozen)) = self.db.read(|c| {
            let env = envs::by_slug(c, project, slug)?;
            // What it handed out that's still out: dynamic credentials not yet revoked (one past
            // its end still works until the sweeper revokes it), and shares not yet used up.
            let leases: Vec<LeaseReach> = c
                .query(
                    "SELECT id, key, consumer, issued_at, expires_at FROM leases WHERE revoked_at IS NULL AND env_id = ? AND path = ? AND kind = 'dynamic' ORDER BY issued_at, id",
                    params![env.id, path],
                    |r| Ok(LeaseReach { id: r.get(0)?, key: r.get(1)?, consumer: r.get(2)?, issued_at: r.get(3)?, expires_at: r.get(4)? }),
                )?
                .into_iter()
                .filter(|l| key.is_none_or(|k| l.key == k))
                .collect();
            let shares: Vec<ShareReach> = c
                .query(
                    "SELECT id, note, key, created_by, expires_at FROM shares WHERE env_id = ? AND path = ? AND ciphertext IS NOT NULL AND expires_at > ? ORDER BY created_at, id",
                    params![env.id, path, now],
                    |r| Ok(ShareReach { id: r.get(0)?, note: r.get(1)?, key: r.get::<_, Option<String>>(2)?.unwrap_or_default(), created_by: r.get(3)?, expires_at: r.get(4)? }),
                )?
                .into_iter()
                .filter(|s| key.is_none_or(|k| s.key == k))
                .collect();
            let unattributed: i64 = c.query_row("SELECT COUNT(*) FROM shares WHERE project_id = ? AND env_id IS NULL AND ciphertext IS NOT NULL AND expires_at > ?", params![project, now], |r| r.get(0))?;
            let frozen = crate::cutoff::freeze_of(c, &env.id, &path)?;
            let held: Held = (leases, shares, unattributed as usize, frozen);
            // As reads resolve them: an importer's importers, an environment based on one, and so
            // on, up to SOURCE_DEPTH steps and SOURCES_MAX folders. A read crosses into another
            // project once, from this project's folder to the one it's shared with (while it's
            // shared): that project's own imports and environments go on from there, but never into
            // a third project (another project's import reads only the folder's own values).
            let mut sources: Vec<Source> = vec![];
            let mut truncated = false;
            let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::from([(env.id.clone(), path.clone())]);
            let mut frontier = vec![(env.id.clone(), project.to_string(), path.clone())];
            for _ in 0..SOURCE_DEPTH {
                let mut next = vec![];
                for (eid, eproject, at) in frontier {
                    let importers: Vec<(String, String, String, String)> = c.query(
                        "SELECT e.id, e.project_id, e.slug, i.path FROM imports i JOIN environments e ON e.id = i.env_id WHERE i.from_env_id = ? AND i.from_path = ? ORDER BY e.project_id, e.slug, i.path",
                        params![eid, at],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                    )?;
                    for (iid, ipid, islug, ipath) in importers {
                        if ipid != eproject && (eproject != project || !crate::exports::exported(c, &eid, &at, &ipid)?) {
                            continue;
                        }
                        if sources.len() >= SOURCES_MAX {
                            truncated = true;
                            break;
                        }
                        if seen.insert((iid.clone(), ipath.clone())) {
                            sources.push((ipid.clone(), islug, ipath.clone()));
                            next.push((iid, ipid, ipath));
                        }
                    }
                    let derived: Vec<(String, String)> = c.query("SELECT id, slug FROM environments WHERE project_id = ? AND base_id = ? ORDER BY slug", params![eproject, eid], |r| Ok((r.get(0)?, r.get(1)?)))?;
                    for (did, dslug) in derived {
                        if sources.len() >= SOURCES_MAX {
                            truncated = true;
                            break;
                        }
                        if seen.insert((did.clone(), at.clone())) {
                            sources.push((eproject.clone(), dslug, at.clone()));
                            next.push((did, eproject.clone(), at.clone()));
                        }
                    }
                }
                frontier = next;
            }
            // One audit read per project (not per folder): its reads in the window, kept for the
            // folders above.
            let mut reads: Vec<(Source, String, i64, String)> = vec![];
            let all: Vec<Source> = std::iter::once((project.to_string(), slug.to_string(), path.clone())).chain(sources.iter().cloned()).collect();
            let mut projects: Vec<&str> = all.iter().map(|s| s.0.as_str()).collect();
            projects.sort();
            projects.dedup();
            for p in projects {
                let wanted: std::collections::HashSet<(&str, &str)> = all.iter().filter(|s| s.0 == p).map(|s| (s.1.as_str(), s.2.as_str())).collect();
                let rows: Vec<(String, String, String, i64, String)> = c.query(
                    "SELECT env, path, actor, at, keys FROM audit WHERE project_id = ? AND at >= ? AND action IN ('secrets.reveal', 'secrets.export', 'secrets.resolve', 'secrets.revalidate', 'sync.push') ORDER BY id DESC LIMIT ?",
                    params![p, now - READ_DAYS * 86_400_000, READS_MAX],
                    |r| Ok((r.get::<_, Option<String>>(0)?.unwrap_or_default(), r.get::<_, Option<String>>(1)?.unwrap_or_default(), r.get(2)?, r.get(3)?, r.get(4)?)),
                )?;
                for (e, at, actor, when, keys) in rows {
                    if wanted.contains(&(e.as_str(), at.as_str())) {
                        reads.push(((p.to_string(), e, at), actor, when, keys));
                    }
                }
            }
            Ok::<_, Error>((env, sources, reads, truncated, held))
        })?;
        let here = |e: &str, p: &str| (e == slug && p == path) || sources.iter().any(|(sp, se, spath)| sp == project && se == e && spath == p);
        let syncs = self
            .list_syncs(project, caller)?
            .into_iter()
            .filter(|s| here(&s.environment, &s.path) && key.is_none_or(|k| s.keys.as_ref().is_none_or(|ks| ks.iter().any(|x| x == k))))
            .map(|s| SyncReach { id: s.id, kind: s.kind, target: s.target, last_synced_at: s.last_synced_at, paused_at: s.paused_at })
            .collect();
        // Another project's folders and environments are its own: named by the project only.
        let mut imported_by: Vec<String> = vec![];
        for (p, e, at) in &sources {
            let name = if p == project { format!("{e}:{at}") } else { format!("project {p}") };
            if !imported_by.contains(&name) {
                imported_by.push(name);
            }
        }
        let mut readers: BTreeMap<String, Reader> = BTreeMap::new();
        for ((sp, se, spath), actor, at, keys) in reads {
            if key.is_some_and(|k| !from_json::<Vec<String>>(&keys).iter().any(|x| x == k)) {
                continue;
            }
            // Another project's readers are its own: named by the project, not their tokens.
            let who = if sp != project {
                format!("project {sp}")
            } else if se == slug && spath == path {
                actor
            } else {
                format!("{actor} (through {se}:{spath})")
            };
            let r = readers.entry(who.clone()).or_insert(Reader { actor: who, last_at: at, reads: 0 });
            r.last_at = r.last_at.max(at);
            r.reads += 1;
        }
        let mut readers: Vec<Reader> = readers.into_values().collect();
        readers.sort_by_key(|r| std::cmp::Reverse(r.last_at));
        Ok(Reach { environment: slug.to_string(), path, key: key.map(String::from), protected: env.protected, untrusted: !env.protected, tokens, identities, syncs, leases, shares, unattributed_shares, projects, imported_by, truncated, readers, uncounted: UNCOUNTED, frozen })
    }
}

//! Reach (docs/secrets.md, "Reach"): every way a folder's values (or one key's) can leave roll,
//! for a security review or before rotating after a leak. Who could (members whose role reveals
//! it, their OAuth clients, tokens and workload identities whose scope covers it, untrusted code
//! when the environment isn't protected), where it goes by itself (syncs, other projects it's
//! shared with, folders that import it, services that read it, sandboxes holding a copy), who
//! actually read it lately (the audit log), and who read it and no longer may. Names and metadata
//! only; never a value.

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
    /// roll-access tokens (`rlt_…`) whose rules reach it.
    pub tokens: Vec<Holder>,
    pub identities: Vec<Holder>,
    /// The project's members whose role reveals it now, decided as their session's requests are.
    pub people: Vec<PersonReach>,
    /// How many accounts are the operator's admins: they reveal everything (`None`: nothing tells).
    pub admins: Option<usize>,
    /// Live OAuth connections (`rlo_…`, a person's MCP client or app) whose rules reach it, by
    /// person (`created_by`). A cut-off's `tokens` revokes them too.
    pub clients: Vec<Holder>,
    /// roll apps services the project stands for whose environment reads it (its own folder, through
    /// a base or an import, or a `roll://` variable), with their last deploy.
    pub services: Vec<ServiceReach>,
    /// Live agents sandboxes whose environment holds a copy of its values.
    pub sandboxes: Vec<SandboxReach>,
    /// People who read its values in the last [`READ_DAYS`] days and may no longer reveal them:
    /// they still know what they read (rotate it).
    pub knew: Vec<Knew>,
    pub syncs: Vec<SyncReach>,
    /// Live dynamic credentials issued for its keys (or the key): their holders keep them until
    /// they end.
    pub leases: Vec<LeaseReach>,
    /// Live placeholders of its keys (or the key), wherever they were resolved (here, a folder
    /// importing it, an environment based on it): roll's proxies swap them for the value
    /// (docs/secrets.md, "Using a key without seeing it").
    pub placeholders: Vec<PlaceholderReach>,
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

/// A member whose role reveals the folder.
#[derive(Debug, Serialize)]
pub struct PersonReach {
    pub user: String,
    pub handle: String,
    /// Their role in the project (`owner`, `admin`, `developer`, `viewer`).
    pub role: String,
    /// Of read, reveal, write and admin: what their role may do here.
    pub actions: Vec<Action>,
    /// An admin account: the operator's equal, whatever the role says.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub admin: bool,
}

/// A service environment that reads the folder.
#[derive(Debug, Serialize)]
pub struct ServiceReach {
    pub service: String,
    pub name: String,
    pub environment: String,
    /// How it reads it: a folder it deploys with (`env:/path`, its own or one importing this one or
    /// based on it), or the `roll://` reference of one of its variables.
    pub through: String,
    /// When what runs now was deployed (`None`: nothing runs; its next deploy reads it).
    pub last_deployed_at: Option<i64>,
}

/// A sandbox whose environment holds values copied from the folder.
#[derive(Debug, Serialize)]
pub struct SandboxReach {
    pub id: String,
    pub name: String,
    /// The roll code owner it runs for.
    pub owner: String,
    /// Who copied the values in (roll secrets' maker: `usr_…`, `token:…`, `operator`).
    pub by: String,
    /// Where they were copied from (`env` or `env:/path`).
    pub from: String,
    /// The copied keys (names only); with `key`, only that one.
    pub keys: Vec<String>,
}

/// Someone who read the folder lately and may no longer reveal it.
#[derive(Debug, Serialize)]
pub struct Knew {
    pub user: String,
    pub last_at: i64,
    pub reads: usize,
    /// `left`: no longer a member; `role`: their role no longer reveals it; `gone`: their account
    /// can't sign in.
    pub why: &'static str,
}

/// What other products hold of a project's secrets, for reach: rolld asks roll apps and roll code
/// (roll secrets knows no product). Names and metadata only, never a value.
pub trait Holders: Send + Sync {
    /// roll apps services the project stands for ([`roll_workspace::Workspace::trusts`]): each
    /// environment, which reads the project's environment of its name (its root folder), with its
    /// variables' `roll://` references (their text, never what they read).
    fn services(&self, project: &str) -> std::result::Result<Vec<ServiceUse>, String>;
    /// Live agents sandboxes holding values copied from the project's secrets.
    fn sandboxes(&self, project: &str) -> std::result::Result<Vec<SandboxCopy>, String>;
}

/// One environment of a roll apps service ([`Holders::services`]).
#[derive(Clone, Debug)]
pub struct ServiceUse {
    pub service: String,
    pub name: String,
    /// The apps environment's name: the project's secrets environment it reads.
    pub environment: String,
    pub last_deployed_at: Option<i64>,
    /// Its variables' `roll://` references.
    pub references: Vec<String>,
}

/// One sandbox's copy ([`Holders::sandboxes`]).
#[derive(Clone, Debug)]
pub struct SandboxCopy {
    pub id: String,
    pub name: String,
    pub owner: String,
    pub by: String,
    /// `env` or `env:/path`.
    pub from: String,
    pub keys: Vec<String>,
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
pub struct PlaceholderReach {
    pub id: String,
    pub key: String,
    /// The consumer it was issued to (`project prj_…` for another project's).
    pub consumer: String,
    /// Who made the consumer, for an in-process one.
    pub maker: Option<String>,
    /// Where it was resolved: `env:/path`, or `project prj_…`.
    pub through: String,
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

/// The people who read a folder lately, by user: when last, how often, and which reading folders
/// (`env`, `path`) they read it through.
type ReadBy = BTreeMap<String, (i64, usize, Vec<(String, String)>)>;

const ACTIONS: [Action; 4] = [Action::Read, Action::Reveal, Action::Write, Action::Admin];

/// What a scope (a token's, an identity's) may do at `slug:path`.
fn here(project: &str, environments: &Option<Vec<String>>, paths: &Option<Vec<String>>, actions: &[Action], slug: &str, path: &str) -> Vec<Action> {
    // As roll-access would judge the same rule (an identity's login gets exactly this one).
    let g = roll_auth::Grant { id: "".into(), name: "".into(), kind: roll_auth::GrantKind::Token, scope: std::sync::Arc::new(roll_auth::scope::Scope(vec![crate::access::rule(project, environments.clone(), paths.clone(), actions)])), user: None, ask_otherwise: false };
    granted(&g, project, slug, path)
}

/// What grant `g` may do at `slug:path`, as roll secrets checks each request of it.
fn granted(g: &roll_auth::Grant, project: &str, slug: &str, path: &str) -> Vec<Action> {
    let g = GrantGrants(g.clone(), Default::default());
    ACTIONS.into_iter().filter(|a| g.allows(*a, &Scope::at(project, slug, path))).collect()
}

impl Secrets {
    /// roll-access's live tokens (not workload logins) with a rule that can name `project`: the
    /// only ones [`Secrets::token_actions`] can find reaching it.
    pub(crate) fn tokens_naming(&self, project: &str) -> Result<Vec<roll_auth::Grant>> {
        match &self.workloads {
            Some(w) => w.grants_naming(project),
            None => Ok(vec![]),
        }
    }

    /// What token `g` may do at `slug:path` of `project`, as each of its requests is judged here: a
    /// person's own token only within their own projects. Empty: it reaches nothing here, and
    /// nothing names it (nor its person). Reach and an environment's delete both ask this.
    pub(crate) fn token_actions(&self, g: &roll_auth::Grant, project: &str, slug: &str, path: &str) -> Vec<Action> {
        let Ok(grants) = self.grant_grants(g) else { return vec![] };
        ACTIONS.into_iter().filter(|a| grants.allows(*a, &Scope::at(project, slug, path))).collect()
    }

    /// Reach of `slug:path` (one key's, with `key`). A project admin's: it lists the project's
    /// credentials, as the tokens list does.
    pub fn reach(&self, project: &str, slug: &str, path: &str, key: Option<&str>, caller: &Caller) -> Result<Reach> {
        caller.need(Action::Admin, &Scope::project(project))?;
        let path = names::path(path)?;
        let now = self.db.now_ms()?;
        // A project's admin cuts off only what stays inside the project; the operator, anything.
        let operator = caller.grants.everything();
        // roll-access's live tokens (not workload logins: those are their identities, below).
        let (clients, tokens): (Vec<_>, Vec<_>) = self
            .tokens_naming(project)?
            .into_iter()
            .filter_map(|g| {
                let actions = self.token_actions(&g, project, slug, &path);
                let skipped = (!operator && !crate::access::confined_to(&g, project)).then_some(BEYOND);
                let oauth = g.kind == roll_auth::GrantKind::Oauth;
                (!actions.is_empty()).then(|| (oauth, Holder { id: g.id.to_string(), name: g.name.to_string(), actions, expires_at: None, last_used_at: None, created_by: g.user.as_deref().unwrap_or("operator").to_string(), disabled_at: None, skipped }))
            })
            .partition(|(oauth, _)| *oauth);
        let tokens: Vec<Holder> = tokens.into_iter().map(|(_, h)| h).collect();
        let mut clients: Vec<Holder> = clients.into_iter().map(|(_, h)| h).collect();
        clients.sort_by(|a, b| (&a.created_by, &a.name).cmp(&(&b.created_by, &b.name)));
        let (people, admins) = self.people_reach(project, slug, &path)?;
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
        type Held = (Vec<LeaseReach>, Vec<PlaceholderReach>, Vec<ShareReach>, usize, Option<crate::cutoff::Freeze>);
        let dead = self.dead_exports("e.project_id = ?", project)?;
        let (env, sources, reads, truncated, (leases, placeholders, shares, unattributed_shares, frozen)) = self.db.read(|c| {
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
            // Placeholders of its keys, by the secret they stand for: wherever they were resolved.
            let placeholders: Vec<PlaceholderReach> = c
                .query(
                    "SELECT p.id, s.key, p.consumer, p.maker, p.project_id, COALESCE(e.slug, ''), p.path, p.expires_at FROM placeholders p JOIN secrets s ON s.id = p.secret_id
                     LEFT JOIN environments e ON e.id = p.env_id WHERE s.env_id = ? AND s.path = ? AND p.revoked_at IS NULL AND p.expires_at > ? ORDER BY p.issued_at, p.id",
                    params![env.id, path, now],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<String>>(3)?, r.get::<_, String>(4)?, r.get::<_, String>(5)?, r.get::<_, String>(6)?, r.get::<_, i64>(7)?)),
                )?
                .into_iter()
                .filter(|p| key.is_none_or(|k| p.1 == k))
                .map(|(id, key, consumer, maker, of, slug, at, expires_at)| {
                    // Another project's consumers are its own: named by the project.
                    if of == project {
                        PlaceholderReach { id, key, consumer, maker, through: format!("{slug}:{at}"), expires_at }
                    } else {
                        PlaceholderReach { id, key, consumer: format!("project {of}"), maker: None, through: format!("project {of}"), expires_at }
                    }
                })
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
            let held: Held = (leases, placeholders, shares, unattributed as usize, frozen);
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
                        "SELECT e.id, e.project_id, e.slug, i.path FROM imports i JOIN environments e ON e.id = i.env_id AND e.forgotten_at IS NULL WHERE i.from_env_id = ? AND i.from_path = ? ORDER BY e.project_id, e.slug, i.path",
                        params![eid, at],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                    )?;
                    for (iid, ipid, islug, ipath) in importers {
                        if ipid != eproject && (eproject != project || !crate::exports::exported(c, &eid, &at, &ipid, &dead)?) {
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
                    "SELECT env, path, actor, at, keys FROM audit WHERE project_id = ? AND at >= ? AND action IN ('secrets.reveal', 'secrets.export', 'secrets.resolve', 'secrets.revalidate', 'secrets.totp', 'sync.push') ORDER BY id DESC LIMIT ?",
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
        let mut read_by: ReadBy = BTreeMap::new();
        for ((sp, se, spath), actor, at, keys) in reads {
            if key.is_some_and(|k| !from_json::<Vec<String>>(&keys).iter().any(|x| x == k)) {
                continue;
            }
            // The person behind a read here (themselves, or their own token), for `knew`.
            if sp == project
                && let Some(user) = self.reader_person(&actor)
            {
                let k = read_by.entry(user).or_insert((at, 0, vec![]));
                k.0 = k.0.max(at);
                k.1 += 1;
                if !k.2.contains(&(se.clone(), spath.clone())) {
                    k.2.push((se.clone(), spath.clone()));
                }
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
        let knew = self.knew(project, slug, &path, read_by)?;
        // Folders of this project whose readers read it: here, and those importing it or based on it.
        let reading: Vec<(&str, &str)> = std::iter::once((slug, path.as_str())).chain(sources.iter().filter(|s| s.0 == project).map(|s| (s.1.as_str(), s.2.as_str()))).collect();
        let (services, sandboxes) = self.held_by(project, key, &reading)?;
        Ok(Reach {
            environment: slug.to_string(),
            path,
            key: key.map(String::from),
            protected: env.protected,
            untrusted: !env.protected,
            tokens,
            identities,
            people,
            admins,
            clients,
            services,
            sandboxes,
            knew,
            syncs,
            leases,
            placeholders,
            shares,
            unattributed_shares,
            projects,
            imported_by,
            truncated,
            readers,
            uncounted: UNCOUNTED,
            frozen,
        })
    }
}

impl Secrets {
    /// The project's members whose role reveals `slug:path` now, as their session's requests are
    /// decided (`GrantGrants` over roll-access's session grant), and how many admin accounts the
    /// operator has.
    fn people_reach(&self, project: &str, slug: &str, path: &str) -> Result<(Vec<PersonReach>, Option<usize>)> {
        let mut people = vec![];
        for m in self.candidates(project)? {
            let Some(person) = self.person(&m.user)? else { continue };
            let actions: Vec<Action> = if person.admin {
                ACTIONS.to_vec()
            } else {
                let session = self.session_grants(&m.user);
                ACTIONS.into_iter().filter(|a| session.allows(*a, &Scope::at(project, slug, path))).collect()
            };
            if actions.contains(&Action::Reveal) {
                people.push(PersonReach { user: m.user, handle: m.handle, role: m.role.as_str().to_string(), actions, admin: person.admin });
            }
        }
        let admins = match &self.people {
            Some(p) => p.admins().map_err(|e| Error::Internal(format!("counting the operator's admins: {e}")))?,
            None => None,
        };
        Ok((people, admins))
    }

    /// Who might reach a project through a role: its members. Members v1.1's groups
    /// (`feature/members-groups`) add the people of the groups that hold a role here; their
    /// session grants already carry the group's rules, so only this list changes.
    fn candidates(&self, project: &str) -> Result<Vec<roll_workspace::members::Member>> {
        match self.workspace.members(roll_workspace::Scope::All, project, false) {
            Ok(m) => Ok(m),
            // A project the workspace no longer has (its recovery window) has no members.
            Err(roll_workspace::Error::NotFound(_)) => Ok(vec![]),
            Err(e) => Err(Error::Internal(e.to_string())),
        }
    }

    /// The person behind an audit row's actor: a person, or a person's own token (`token:…`, as
    /// roll-access still knows it after it ends).
    fn reader_person(&self, actor: &str) -> Option<String> {
        if crate::access::is_person(actor) {
            return Some(actor.to_string());
        }
        let id = actor.strip_prefix("token:")?;
        self.grants.as_ref()?.owner(id).flatten()
    }

    /// Of the people who read the folder lately (`read_by`: when last, how often, which reading
    /// folders), those who may reveal neither it nor where they read it now.
    fn knew(&self, project: &str, slug: &str, path: &str, read_by: ReadBy) -> Result<Vec<Knew>> {
        let mut knew = vec![];
        for (user, (last_at, reads, at)) in read_by {
            let why = match self.person(&user)? {
                None => "gone",
                Some(p) if p.admin => continue,
                Some(_) => {
                    let session = self.session_grants(&user);
                    let reveals = |e: &str, p: &str| session.allows(Action::Reveal, &Scope::at(project, e, p));
                    if reveals(slug, path) || at.iter().any(|(e, p)| reveals(e, p)) {
                        continue;
                    }
                    if session.sees(project) { "role" } else { "left" }
                }
            };
            knew.push(Knew { user, last_at, reads, why });
        }
        knew.sort_by_key(|k| std::cmp::Reverse(k.last_at));
        Ok(knew)
    }

    /// Services and sandboxes that read a folder (one key's, with `key`): through rolld's
    /// [`Holders`]. `reading`: this project's folders whose readers read it (the folder itself
    /// first). Nothing without a provider (roll apps and roll code aren't on this server).
    fn held_by(&self, project: &str, key: Option<&str>, reading: &[(&str, &str)]) -> Result<(Vec<ServiceReach>, Vec<SandboxReach>)> {
        let Some(h) = self.reach_holders.get() else { return Ok((vec![], vec![])) };
        let reads = |e: &str, p: &str| reading.iter().any(|(re, rp)| *re == e && *rp == p);
        let mut services = vec![];
        for s in h.services(project).map_err(|e| Error::Internal(format!("listing roll apps' services: {e}")))? {
            // Its environment's root folder, as a deploy reads it.
            let mut through = reads(&s.environment, "/").then(|| format!("{}:/", s.environment));
            if through.is_none() {
                through = s.references.iter().find(|r| match crate::consumers::Reference::parse(r) {
                    // Its key read there: this folder's, or an importing one's that may be this folder's.
                    Ok(Some(r)) => r.project.as_deref().is_none_or(|p| p == project) && reads(&r.env, &r.path) && key.is_none_or(|k| k == r.key),
                    _ => false,
                }).cloned();
            }
            if let Some(through) = through {
                services.push(ServiceReach { service: s.service, name: s.name, environment: s.environment, through, last_deployed_at: s.last_deployed_at });
            }
        }
        let mut sandboxes = vec![];
        for c in h.sandboxes(project).map_err(|e| Error::Internal(format!("listing agents' sandboxes: {e}")))? {
            let (e, p) = c.from.split_once(':').unwrap_or((c.from.as_str(), "/"));
            if !reads(e, p) {
                continue;
            }
            let keys: Vec<String> = c.keys.iter().filter(|k| key.is_none_or(|x| x == k.as_str())).cloned().collect();
            if keys.is_empty() {
                continue;
            }
            sandboxes.push(SandboxReach { id: c.id, name: c.name, owner: c.owner, by: c.by, from: c.from, keys });
        }
        Ok((services, sandboxes))
    }

    /// Where reach asks which services and sandboxes hold a project's secrets (rolld, once roll
    /// apps and roll code are open).
    pub fn set_reach_holders(&self, holders: std::sync::Arc<dyn Holders>) {
        let _ = self.reach_holders.set(holders);
    }
}

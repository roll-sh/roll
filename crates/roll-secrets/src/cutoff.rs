//! An incident's cut-off (docs/secrets.md, "Reach" and "During an incident"): everything
//! [`Reach`](crate::reach::Reach) lists that can read a folder (or one key) is cut in one call,
//! and the answer says what was cut and what couldn't be, and why.
//!
//! - **Tokens** (roll-access's grants reach lists) are revoked through [`Workloads::revoke_many`]
//!   (rolld: `Access::revoke_many`), at most [`REVOKE_MAX`] a call: all or none per call. A
//!   project's admin revokes only tokens confined to the project
//!   ([`confined_to`](crate::access::confined_to)); one that reaches beyond it (the operator's
//!   all-projects token, say) is listed as `skipped`, for the operator, who revokes any of them.
//! - **Identities** that reach it are disabled (they don't log in until an admin enables them),
//!   and their live logins' grants go through `revoke_many` too.
//! - **Leases** on its keys end, their credentials revoked outside the transaction, as ending one
//!   does.
//! - **Placeholders** of its keys end (in the transaction), freeze or not: roll's proxies stop
//!   swapping them at once.
//! - **Shares**: one-time shares made from its keys expire, and the projects it's shared with
//!   stop reading it (its exports are removed).
//! - **Syncs** that push it are paused until an admin resumes them.
//! - **Freeze** (optional): reads of its values answer 423 `frozen` to everyone but its admins,
//!   who can still read and write, so they can rotate. An admin here is the operator, a signed-in
//!   person or a roll-access token with `admin` there ([`Caller::reads_frozen`]): never roll's own
//!   readers (apps' services, checks, sandboxes, previews, other products' references), a sync, a
//!   workload identity's login or a share link, whatever they may otherwise do.
//!
//! What's roll secrets' own (identities, shares, exports, syncs, the freeze) changes in one
//! transaction; revocations happen outside it. One audit row names what was cut and why. A second
//! call cuts what's left.
//!
//! [`Workloads::revoke_many`]: crate::access::Workloads::revoke_many

use crate::access::{Action, Caller, REVOKE_MAX, Scope};
use crate::{Error, Result, Secrets, audit, envs, names};
use code_sql::{Conn, OptionalExtension, params};
use serde::{Deserialize, Serialize};

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS freezes (
    env_id TEXT NOT NULL,
    path TEXT NOT NULL,
    why TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    PRIMARY KEY (env_id, path)
) WITHOUT ROWID;
";

/// A frozen folder: why, when and by whom.
#[derive(Debug, Serialize, Clone)]
pub struct Freeze {
    pub why: String,
    pub at: i64,
    pub by: String,
}

/// Why `env_id`'s `path` is frozen, if it is (inside a read).
pub(crate) fn frozen(c: &Conn<'_>, env_id: &str, path: &str) -> Result<Option<String>> {
    Ok(c.query_row("SELECT why FROM freezes WHERE env_id = ? AND path = ?", [env_id, path], |r| r.get(0)).optional()?)
}

pub(crate) fn freeze_of(c: &Conn<'_>, env_id: &str, path: &str) -> Result<Option<Freeze>> {
    Ok(c.query_row("SELECT why, created_at, created_by FROM freezes WHERE env_id = ? AND path = ?", [env_id, path], |r| Ok(Freeze { why: r.get(0)?, at: r.get(1)?, by: r.get(2)? })).optional()?)
}

fn yes() -> bool {
    true
}

/// `POST …/environments/{env}/reach/cut-off?path=&key=`: what to cut, and why. Everything but
/// the freeze by default.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CutOffRequest {
    pub why: String,
    #[serde(default = "yes")]
    pub tokens: bool,
    #[serde(default = "yes")]
    pub identities: bool,
    #[serde(default = "yes")]
    pub leases: bool,
    #[serde(default = "yes")]
    pub placeholders: bool,
    #[serde(default = "yes")]
    pub shares: bool,
    #[serde(default = "yes")]
    pub syncs: bool,
    #[serde(default)]
    pub freeze: bool,
}

/// One thing cut: its id and a name to show.
#[derive(Debug, Serialize)]
pub struct Cut {
    pub id: String,
    pub name: String,
}

/// A token a cut-off left alone, and why (it reaches beyond the project: the operator's to revoke).
#[derive(Debug, Serialize)]
pub struct Skipped {
    pub id: String,
    pub name: String,
    pub why: &'static str,
}

/// What couldn't be cut, and why; the rest went on.
#[derive(Debug, Serialize)]
pub struct Failure {
    /// `tokens`, `identities`, `logins`, `leases`, `placeholders`, `shares`, `syncs`, `freeze` or `audit`.
    pub what: &'static str,
    pub ids: Vec<String>,
    pub why: String,
}

/// What a cut-off did.
#[derive(Debug, Serialize)]
pub struct CutOff {
    pub environment: String,
    pub path: String,
    pub key: Option<String>,
    pub why: String,
    pub tokens: Vec<Cut>,
    /// Tokens that reach it but beyond the project too, left for the operator to revoke.
    pub skipped: Vec<Skipped>,
    /// Identities disabled now.
    pub identities: Vec<Cut>,
    /// Live logins of the identities that reach it, revoked.
    pub logins: usize,
    /// How many calls to roll-access the tokens and logins took (at most 500 grants a call).
    pub batches: usize,
    pub leases: Vec<Cut>,
    /// Placeholders of its keys, ended.
    pub placeholders: Vec<Cut>,
    pub shares: Vec<Cut>,
    /// Projects that no longer read it (its exports removed).
    pub projects: Vec<String>,
    pub syncs: Vec<Cut>,
    /// The folder is frozen (now or already).
    pub frozen: bool,
    pub failed: Vec<Failure>,
}

impl CutOff {
    fn summary(&self) -> String {
        let n = |k: usize, one: &str, many: &str| (k > 0).then(|| format!("{k} {}", if k == 1 { one } else { many }));
        let parts: Vec<String> = [
            n(self.tokens.len(), "token", "tokens"),
            n(self.identities.len(), "identity", "identities"),
            n(self.logins, "login", "logins"),
            n(self.leases.len(), "lease", "leases"),
            n(self.placeholders.len(), "placeholder", "placeholders"),
            n(self.shares.len(), "share", "shares"),
            n(self.projects.len(), "project's import", "projects' imports"),
            n(self.syncs.len(), "sync", "syncs"),
        ]
        .into_iter()
        .flatten()
        .collect();
        let cut = if parts.is_empty() { "nothing left to cut".to_string() } else { format!("cut {}", parts.join(", ")) };
        let frozen = if self.frozen { ", frozen" } else { "" };
        let skipped = match self.skipped.len() {
            0 => String::new(),
            1 => "; 1 token reaches beyond the project, left to the operator".into(),
            k => format!("; {k} tokens reach beyond the project, left to the operator"),
        };
        let failed = if self.failed.is_empty() { String::new() } else { format!("; failed: {}", self.failed.iter().map(|f| f.what).collect::<Vec<_>>().join(", ")) };
        format!("{cut}{frozen}{skipped}{failed}: {}", self.why).chars().take(1000).collect()
    }
}

/// The ids one bulk revoke refused, and why.
type Refused = (Vec<String>, String);

impl Secrets {
    /// Cut off everything that can read `slug:path` (or its `key`): see the module's doc. A
    /// project admin's, as reach is; tokens reaching beyond the project only the operator's.
    pub fn cut_off(&self, project: &str, slug: &str, path: &str, key: Option<&str>, req: CutOffRequest, caller: &Caller) -> Result<CutOff> {
        caller.need(Action::Admin, &Scope::project(project))?;
        let why: String = req.why.chars().filter(|c| !c.is_control()).collect::<String>().trim().chars().take(300).collect();
        if why.is_empty() {
            return Err(Error::BadRequest("say why (the incident): it's on every revoked token and in the audit log".into()));
        }
        let path = names::path(path)?;
        // As the caller sees it: the same list the confirmation showed.
        let reach = self.reach(project, slug, &path, key, caller)?;
        let mut out = CutOff {
            environment: slug.to_string(),
            path: path.clone(),
            key: key.map(String::from),
            why: why.clone(),
            tokens: vec![],
            skipped: vec![],
            identities: vec![],
            logins: 0,
            batches: 0,
            leases: vec![],
            placeholders: vec![],
            shares: vec![],
            projects: vec![],
            syncs: vec![],
            frozen: reach.frozen.is_some(),
            failed: vec![],
        };
        let by = caller.principal.clone();
        let reason = format!("cut off from {slug}:{path}{}: {why}", key.map(|k| format!(" {k}")).unwrap_or_default());

        // 1. Tokens first: they're what an attacker holding one uses now. Reach marked those this
        // caller may not revoke (they reach other projects too): they're listed, not revoked.
        // OAuth connections (`clients`) are roll-access grants too: cut with the tokens.
        if req.tokens && !(reach.tokens.is_empty() && reach.clients.is_empty()) {
            let (named, skipped): (Vec<_>, Vec<_>) = reach.tokens.iter().chain(&reach.clients).partition(|h| h.skipped.is_none());
            out.skipped = skipped.into_iter().map(|h| Skipped { id: h.id.clone(), name: h.name.clone(), why: h.skipped.unwrap_or(crate::reach::BEYOND) }).collect();
            let named: Vec<(String, String)> = named.into_iter().map(|h| (h.id.clone(), h.name.clone())).collect();
            let (done, failed) = if named.is_empty() { (vec![], vec![]) } else { self.revoke_grants(&named, &reason, &by, &mut out.batches) };
            out.tokens = done.into_iter().map(|(id, name)| Cut { id, name }).collect();
            out.failed.extend(failed.into_iter().map(|(ids, why)| Failure { what: "tokens", ids, why }));
        }

        // 2. roll secrets' own, in one transaction: identities disabled (and their live logins
        // read), shares expired, exports removed, syncs paused, the folder frozen.
        let now = self.db.now_ms()?;
        let disable: Vec<&str> = if req.identities { reach.identities.iter().map(|h| h.id.as_str()).collect() } else { vec![] };
        let share_ids: Vec<&str> = if req.shares { reach.shares.iter().map(|s| s.id.as_str()).collect() } else { vec![] };
        let projects: Vec<&str> = if req.shares { reach.projects.iter().map(String::as_str).collect() } else { vec![] };
        let sync_ids: Vec<&str> = if req.syncs { reach.syncs.iter().filter(|s| s.paused_at.is_none()).map(|s| s.id.as_str()).collect() } else { vec![] };
        let placeholder_ids: Vec<&str> = if req.placeholders { reach.placeholders.iter().map(|p| p.id.as_str()).collect() } else { vec![] };
        let own = self.db.write(|c| {
            let env = envs::by_slug(c, project, slug)?;
            let mut disabled = vec![];
            let mut logins: Vec<String> = vec![];
            for id in &disable {
                if c.execute("UPDATE identities SET disabled_at = ?, disabled_why = ? WHERE id = ? AND project_id = ? AND disabled_at IS NULL", params![now, why, id, project])? > 0 {
                    disabled.push(id.to_string());
                }
                // Already disabled or not: a login a first call couldn't revoke goes now.
                logins.extend(c.query("SELECT grant_id FROM identity_logins WHERE identity = ? AND expires_at > ?", params![id, now], |r| r.get::<_, String>(0))?);
            }
            let mut expired = vec![];
            for id in &share_ids {
                if c.execute("UPDATE shares SET ciphertext = NULL, views_left = 0 WHERE id = ? AND project_id = ? AND ciphertext IS NOT NULL", [id, &project])? > 0 {
                    expired.push(id.to_string());
                }
            }
            let mut unshared = vec![];
            for p in &projects {
                if c.execute("DELETE FROM exports WHERE env_id = ? AND path = ? AND to_project = ?", [env.id.as_str(), path.as_str(), p])? > 0 {
                    unshared.push(p.to_string());
                }
            }
            let mut paused = vec![];
            for id in &sync_ids {
                if c.execute("UPDATE syncs SET paused_at = ?, paused_why = ? WHERE id = ? AND project_id = ? AND paused_at IS NULL", params![now, why, id, project])? > 0 {
                    paused.push(id.to_string());
                }
            }
            let mut ended = vec![];
            for id in &placeholder_ids {
                if c.execute("UPDATE placeholders SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL", params![now, id])? > 0 {
                    ended.push(id.to_string());
                }
            }
            if req.freeze && frozen(c, &env.id, &path)?.is_none() {
                c.execute("INSERT INTO freezes (env_id, path, why, created_at, created_by) VALUES (?, ?, ?, ?, ?)", params![env.id, path, why, now, by])?;
            }
            Ok::<_, Error>((disabled, logins, expired, unshared, paused, ended))
        });
        let logins = match own {
            Ok((disabled, logins, expired, unshared, paused, ended)) => {
                out.placeholders = ended.iter().map(|id| Cut { id: id.clone(), name: reach.placeholders.iter().find(|p| &p.id == id).map(|p| format!("{} for {}", p.key, p.consumer)).unwrap_or_default() }).collect();
                // Their consumers' interception CAs naming a host they no longer have end.
                self.hosts_shrank(reach.placeholders.iter().filter(|p| ended.contains(&p.id)).map(|p| p.consumer.as_str()));
                let name = |list: &[crate::reach::Holder], id: &str| list.iter().find(|h| h.id == id).map(|h| h.name.clone()).unwrap_or_default();
                out.identities = disabled.iter().map(|id| Cut { id: id.clone(), name: name(&reach.identities, id) }).collect();
                out.shares = expired.iter().map(|id| Cut { id: id.clone(), name: reach.shares.iter().find(|s| &s.id == id).map(|s| s.note.clone()).unwrap_or_default() }).collect();
                out.projects = unshared;
                out.syncs = paused.iter().map(|id| Cut { id: id.clone(), name: reach.syncs.iter().find(|s| &s.id == id).map(|s| s.kind.clone()).unwrap_or_default() }).collect();
                out.frozen |= req.freeze;
                logins
            }
            Err(e) => {
                let why = e.to_string();
                for (what, on) in [("identities", req.identities), ("placeholders", req.placeholders), ("shares", req.shares), ("syncs", req.syncs), ("freeze", req.freeze)] {
                    if on {
                        out.failed.push(Failure { what, ids: vec![], why: why.clone() });
                    }
                }
                vec![]
            }
        };

        // 3. The disabled identities' live logins: the same bulk revoke.
        let logins: Vec<String> = logins.into_iter().filter(|id| !out.tokens.iter().any(|t| &t.id == id)).collect();
        if !logins.is_empty() {
            let named: Vec<(String, String)> = logins.into_iter().map(|id| (id.clone(), id)).collect();
            let (done, failed) = self.revoke_grants(&named, &reason, &by, &mut out.batches);
            out.logins = done.len();
            out.failed.extend(failed.into_iter().map(|(ids, why)| Failure { what: "logins", ids, why }));
        }

        // 4. Leases: each claimed in a write, its credential revoked outside it (as ending one).
        if req.leases {
            for l in &reach.leases {
                match self.revoke_one(&l.id) {
                    Ok(true) => out.leases.push(Cut { id: l.id.clone(), name: format!("{} for {}", l.key, l.consumer) }),
                    Ok(false) => {}
                    Err(e) => out.failed.push(Failure { what: "leases", ids: vec![l.id.clone()], why: e.to_string() }),
                }
            }
        }

        // 5. One audit row for the whole action (revocations write their own as well).
        let keys: Vec<String> = key.map(String::from).into_iter().collect();
        let summary = out.summary();
        if let Err(e) = self.db.now_ms().map_err(Error::from).and_then(|now| self.db.write(|c| audit::record(c, self.keys()?, project, caller, "reach.cut_off", Some(slug), Some(&path), &keys, &summary, now))) {
            tracing::error!(%project, "recording a cut-off's audit row failed: {e}");
            out.failed.push(Failure { what: "audit", ids: vec![], why: e.to_string() });
        }
        Ok(out)
    }

    /// Revoke `grants` (id, name) through roll-access, [`REVOKE_MAX`] a call. What was revoked,
    /// and each call that failed with why (all or none per call).
    fn revoke_grants(&self, grants: &[(String, String)], why: &str, by: &str, calls: &mut usize) -> (Vec<(String, String)>, Vec<Refused>) {
        let Some(w) = &self.workloads else {
            return (vec![], vec![(grants.iter().map(|g| g.0.clone()).collect(), "roll-access isn't on this server".into())]);
        };
        let (mut done, mut failed) = (vec![], vec![]);
        for chunk in grants.chunks(REVOKE_MAX) {
            let ids: Vec<String> = chunk.iter().map(|g| g.0.clone()).collect();
            *calls += 1;
            match w.revoke_many(&ids, why, by) {
                Ok(()) => done.extend(chunk.iter().cloned()),
                Err(e) => failed.push((ids, e.to_string())),
            }
        }
        (done, failed)
    }

    /// Lift a folder's freeze (`DELETE …/reach/cut-off?path=`): a project admin's.
    pub fn unfreeze(&self, project: &str, slug: &str, path: &str, caller: &Caller) -> Result<()> {
        caller.need(Action::Admin, &Scope::project(project))?;
        let path = names::path(path)?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let env = envs::by_slug(c, project, slug)?;
            let Some(f) = freeze_of(c, &env.id, &path)? else {
                return Err(Error::NotFound(format!("{slug}:{path} isn't frozen")));
            };
            c.execute("DELETE FROM freezes WHERE env_id = ? AND path = ?", [env.id.as_str(), path.as_str()])?;
            audit::record(c, self.keys()?, project, caller, "reach.unfreeze", Some(slug), Some(&path), &[], &format!("unfroze {slug}:{path} (frozen by {}: {})", f.by, f.why), now)
        })
    }
}

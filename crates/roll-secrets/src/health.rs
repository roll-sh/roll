//! A project's health (docs/secrets.md, "Health"): everything insights flag, in every standard
//! environment and folder at once, worst first, so nothing waits on someone opening the right
//! folder. Each finding names a key where it's set, never a value; nothing is decrypted.

use crate::access::{Action, Caller, Scope};
use crate::envs::{self, Kind};
use crate::insights::EXPIRING_DAYS;
use crate::{Result, Secrets, from_json};
use code_sql::params;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

/// Reads of values over this many days count as use.
const UNREAD_DAYS: i64 = 90;
const READS_MAX: i64 = 100_000;

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Issue {
    /// A decoy key's credential was used (docs/secrets.md, "Decoys"): someone found it where it
    /// was planted. Only the project's admins see it, signed in or the operator, never a token.
    DecoyUsed,
    /// The project's `production` isn't protected (a project made before new ones started with it
    /// protected, or unprotected by hand): checks, sandboxes and untrusted previews read it, and
    /// nothing there needs a change request. `detail` lists what reaches it: members by role,
    /// checks and sandboxes that read it in the last 90 days. `key` is empty. Never changed for
    /// the project: protecting it is the admin's call (the dashboard's "Protect production").
    ProductionUnprotected,
    /// Someone who may no longer back the project up took a backup of it in its history
    /// (`history_days`, else 90 days) that carried registrations' keys (a CA's, a vendor's
    /// credential, a DNS token): they can restore those anywhere. `key` is the person, `detail`
    /// what each backup carried and what to do (re-issue a CA, rotate a credential); `environment`
    /// is empty. Only for the project's admins. Gone once those backups are older than its history.
    BackupLeft,
    /// The project is below roll secrets' policy (guardrails.rs), as it was before the policy or
    /// a floor was raised: nothing is rewritten. `detail` says the floor and how far it is:
    /// `production` (`key` empty), `leaks.rotate` (`environment` and `key` empty), or a sync of
    /// a kind the policy leaves out, or storing a cloud key under `no_stored_cloud_keys` (`key`
    /// the sync's id, which then isn't `stored_cloud_key` too).
    OffPolicy,
    /// Its value stopped working outside roll. A SaaS rotator's registered credential too (`key`
    /// its name, `environment` empty): roll can't issue or revoke its keys any more.
    Expired,
    /// Its scheduled rotation fails (the database, vendor or rotator refused or didn't answer).
    RotationFailing,
    /// A retired or expired credential roll can't revoke yet (it keeps trying), or a public
    /// certificate's revocation the CA hasn't taken (`key` its name, `environment` empty).
    RevokeFailing,
    /// A required key an environment doesn't define.
    Missing,
    /// A protected environment's value, also set in an unprotected one; or a public certificate
    /// linked into a protected environment whose private key is also linked into one that isn't.
    Exposed,
    /// A server's credential under a name a framework puts in the browser bundle
    /// (`NEXT_PUBLIC_`, `VITE_`…): whoever loads the site can read it.
    Public,
    /// A credential easy to guess (shown to whoever may reveal it).
    Weak,
    /// A key bound to hosts with no header or query parameter (bound before a placement was
    /// required): a trusted reader's placeholder may ride in any header, one the host might echo,
    /// and an untrusted consumer's doesn't swap.
    Unplaced,
    /// A sync or a workload identity whose maker is the operator account: made from an admin's
    /// session before makers were people, so it would act after they're demoted or can't sign
    /// in. Re-saving it from a person's session binds it to them. `key` is the sync's or the
    /// identity's id (the identity's `environment` and `path` are empty).
    OperatorMade,
    /// A sync that signs in to AWS, Google or Azure with a cloud key stored in roll where it could
    /// sign in without one (the project's AWS role, workload identity federation, a federated
    /// credential: docs/secrets.md, "Syncing without a cloud key"). A recommendation: the key
    /// works, but it can leak, expire and needs rotating. `key` is the sync's id, `detail` what to
    /// use instead.
    StoredCloudKey,
    /// Its value stops working outside roll within two weeks (a credential a vendor ends by
    /// itself, such as a GitLab token, that roll hasn't rotated by then), or a SaaS rotator's
    /// registered credential does (`credential_expires_at`; `key` its name, `environment` empty).
    Expiring,
    /// A certificate a project CA signed for a CSR ends within two weeks (pki.rs). Links' own
    /// certificates are replaced by roll; a CSR's is the requester's to renew. `key` is its
    /// names, `path` the CA's name, `environment` empty.
    CertificateExpiring,
    /// Unchanged for longer than the environment's `max_age_days`.
    Stale,
    /// Nobody read it in 90 days.
    Unread,
    /// No repository on the project's canvas mentions it (health's `code` check).
    Unreferenced,
}

/// roll code's search, as health's `code` check needs it (rolld implements it with roll code's
/// trigram index): roll secrets reads no files.
pub trait CodeRefs: Send + Sync {
    /// Which of `keys` appear on repository `repo`'s (`owner/name`) default branch, anywhere, case
    /// sensitive (a substring is enough: a key that might be read is never flagged). `None`: the
    /// repository can't be searched here (not indexed yet, roll code off), so nothing is said.
    fn referenced(&self, repo: &str, keys: &[String]) -> Option<BTreeSet<String>>;
}

/// The most keys one `code` check looks for.
pub const CODE_KEYS_MAX: usize = 500;

#[derive(Debug, Serialize, PartialEq)]
pub struct Finding {
    pub issue: Issue,
    pub environment: String,
    pub path: String,
    pub key: String,
    /// What exactly (the other environments, the reason, the date).
    pub detail: String,
    /// Who supplies the key, when a required key names them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// A `decoy_used` finding's decoy (only [`Secrets::sees_decoys`] callers get the finding): its
    /// uses and the last one's where. Never in a reminder: that lists issue, path, key and detail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decoy: Option<crate::decoys::Decoy>,
}

/// One project's findings, for health across projects.
#[derive(Debug, Serialize)]
pub struct ProjectFindings {
    pub project: String,
    pub project_name: String,
    pub findings: Vec<Finding>,
}

impl Secrets {
    /// Where roll code's search comes from (rolld, once roll code is open).
    pub fn set_code_refs(&self, refs: Arc<dyn CodeRefs>) {
        let _ = self.code_refs.set(refs);
    }

    /// Every project's findings the caller may read (`GET /v1/secrets/findings`), projects with
    /// none left out: what Home and `roll secrets health --all` show.
    pub fn all_health(&self, caller: &Caller) -> Result<Vec<ProjectFindings>> {
        let projects: Vec<String> = match caller.grants.project() {
            Some(p) => vec![p.to_string()],
            None => self.db.read(|c| c.query("SELECT project_id FROM projects ORDER BY project_id", (), |r| r.get(0)))?,
        };
        let mut out = vec![];
        for project in projects {
            if !caller.allows(Action::Read, &Scope::project(&project)) {
                continue;
            }
            let findings = match self.project_health(&project, caller, false) {
                Ok(f) => f,
                // A project gone from the workspace, or one the caller can't read after all.
                Err(_) => continue,
            };
            if findings.is_empty() {
                continue;
            }
            let project_name = self.workspace.get(roll_workspace::Scope::All, &project).map(|p| p.name).unwrap_or_else(|_| project.clone());
            out.push(ProjectFindings { project, project_name, findings });
        }
        Ok(out)
    }

    /// Every finding in the project's standard environments the caller may read, worst first.
    /// With `code`, keys no repository on the project's canvas mentions too (`unreferenced`):
    /// a search per key and repository, so only when asked for.
    pub fn project_health(&self, project: &str, caller: &Caller, code: bool) -> Result<Vec<Finding>> {
        self.findings(project, caller, code, true)
    }

    /// [`Self::project_health`]. `exempt`: decoys left out of `stale` and `unread` for whoever sees
    /// them (nobody should rotate or read one). Reminders pass `false`: a channel lists a decoy's
    /// staleness as any key's, so nothing is learned there by elimination.
    pub(crate) fn findings(&self, project: &str, caller: &Caller, code: bool, exempt: bool) -> Result<Vec<Finding>> {
        caller.need(Action::Read, &Scope::project(project))?;
        let dek = self.project(project)?;
        let now = self.db.now_ms()?;
        let empty = dek.fingerprint("");
        type Row = (String, String, String, String, Option<String>, Option<String>, i64, Option<i64>, Option<String>, Option<String>);
        let (envs, rows, reads) = self.db.read(|c| {
            let envs: Vec<envs::EnvRow> = envs::all(c, project)?.into_iter().filter(|e| e.kind == Kind::Standard).collect();
            let rows: Vec<Row> = c.query(
                "SELECT env_id, path, key, fp, link, refs, updated_at, expires_at, weak, public FROM secrets WHERE project_id = ? AND owner = '' ORDER BY path, key",
                [project],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?)),
            )?;
            let reads: Vec<(String, String, String)> = c.query(
                "SELECT env, path, keys FROM audit WHERE project_id = ? AND at >= ? AND action IN ('secrets.reveal', 'secrets.export', 'secrets.resolve', 'secrets.revalidate', 'secrets.totp', 'sync.push') ORDER BY id DESC LIMIT ?",
                params![project, now - UNREAD_DAYS * 86_400_000, READS_MAX],
                |r| Ok((r.get::<_, Option<String>>(0)?.unwrap_or_default(), r.get::<_, Option<String>>(1)?.unwrap_or_default(), r.get(2)?)),
            )?;
            Ok::<_, crate::Error>((envs, rows, reads))
        })?;
        let slug: HashMap<&str, &envs::EnvRow> = envs.iter().map(|e| (e.id.as_str(), e)).collect();
        // Decoys, for the admins: their use first, and no staleness or disuse (nobody should read one).
        let admin = self.sees_decoys(project, caller);
        let used = if admin { self.used_decoys(project)? } else { vec![] };
        let decoys: HashSet<(String, String, String)> = if admin && exempt { self.live_decoys(project)? } else { HashSet::new() };
        let mut read: HashSet<(String, String, String)> = HashSet::new();
        for (env, path, keys) in reads {
            for k in from_json::<Vec<String>>(&keys) {
                read.insert((env.clone(), path.clone(), k));
            }
        }
        // Plain values of unprotected environments, by fingerprint: where a protected one's also is.
        // (environment, folder) of each: access is checked where the copy actually is.
        let mut unprotected: HashMap<&str, BTreeSet<(&str, &str)>> = HashMap::new();
        for (env_id, at, _, fp, link, refs, ..) in &rows {
            if let Some(e) = slug.get(env_id.as_str())
                && !e.protected
                && link.is_none()
                && refs.as_deref().is_none_or(|r| r == "[]")
                && *fp != empty
            {
                unprotected.entry(fp.as_str()).or_default().insert((e.slug.as_str(), at.as_str()));
            }
        }
        let mut out = vec![];
        for d in used {
            // An ephemeral environment's (or a deleted one's) shows where the decoy was planted.
            let env = slug.get(d.env_id.as_str()).map_or_else(|| d.env_id.clone(), |e| e.slug.clone());
            let u = &d.decoy;
            let from = u.used_ip.as_deref().map(|i| format!(" from {i}")).unwrap_or_default();
            let on = match (&u.used_route, &u.used_endpoint) {
                (_, Some(ep)) => format!(" on Postgres endpoint {ep}"),
                (Some(route), None) => format!(" calling {route}"),
                (None, None) => String::new(),
            };
            let times = if u.uses == 1 { "once".to_string() } else { format!("{} times", u.uses) };
            let day = u.used_at.map(chrono_day).unwrap_or_default();
            let detail = format!("used {times}, last on {day}{from}{on}: cut it off and rotate what sits beside it");
            out.push(Finding { issue: Issue::DecoyUsed, environment: env, path: d.path, key: d.key, detail, owner: None, decoy: Some(d.decoy) });
        }
        if let Some(e) = envs.iter().find(|e| e.slug == crate::PRODUCTION && !e.protected)
            && caller.allows(Action::Read, &Scope::env(project, &e.slug))
        {
            let detail = self.production_reach(project, now)?;
            out.push(Finding { issue: Issue::ProductionUnprotected, environment: e.slug.clone(), path: "/".into(), key: String::new(), detail, owner: None, decoy: None });
        }
        if caller.allows(Action::Admin, &Scope::whole(project)) {
            out.extend(self.backups_left(project, now)?);
        }
        // How far the project is from roll secrets' policy.
        let floors = self.floors()?;
        out.extend(self.off_policy(project, caller, &floors, &envs)?);
        let mut have: HashSet<(String, String, String)> = HashSet::new();
        for (env_id, path, key, fp, link, refs, updated_at, expires_at, weak, public) in &rows {
            let Some(e) = slug.get(env_id.as_str()) else { continue };
            have.insert((e.slug.clone(), path.clone(), key.clone()));
            if !caller.allows(Action::Read, &Scope::at(project, &e.slug, path)) {
                continue;
            }
            let mut add = |issue: Issue, detail: String| out.push(Finding { issue, environment: e.slug.clone(), path: path.clone(), key: key.clone(), detail, owner: None, decoy: None });
            let day = |ms: i64| chrono_day(ms);
            if let Some(t) = expires_at {
                if *t <= now {
                    add(Issue::Expired, format!("expired {}", day(*t)));
                } else if *t < now + EXPIRING_DAYS * 86_400_000 {
                    add(Issue::Expiring, format!("expires {}", day(*t)));
                }
            }
            if let Some(w) = weak
                && caller.allows(Action::Reveal, &Scope::at(project, &e.slug, path))
            {
                add(Issue::Weak, w.clone());
            }
            // Judged at write by the value; a link's secret field (or an older row's name) here, and
            // only for a name a framework sends to browsers.
            let linked = || link.as_deref().and_then(|l| serde_json::from_str::<crate::Link>(l).ok());
            let judged = || crate::strength::browser_exposure(key, None, self.secret_field(linked().as_ref()).as_deref());
            if let Some(p) = public.clone().or_else(|| crate::strength::browser_named(key).then(judged).flatten()) {
                // What kind of credential it is comes from the value: only for whoever may reveal it.
                let shown = if caller.allows(Action::Reveal, &Scope::at(project, &e.slug, path)) { p } else { crate::strength::public_without_reveal(&p) };
                add(Issue::Public, shown);
            }
            if e.protected
                && link.is_none()
                && refs.as_deref().is_none_or(|r| r == "[]")
                && *fp != empty
                && let Some(others) = unprotected.get(fp.as_str())
            {
                let named: Vec<&str> = others.iter().filter(|(o, at)| caller.allows(Action::Read, &Scope::at(project, o, at))).map(|(o, _)| *o).collect::<BTreeSet<_>>().into_iter().collect();
                if !named.is_empty() {
                    add(Issue::Exposed, format!("also in {}", named.join(", ")));
                }
            }
            let decoy = decoys.contains(&(env_id.clone(), path.clone(), key.clone()));
            let changes_alone = !decoy && link.as_deref().is_none_or(|l| l.contains("\"mode\":\"managed\""));
            if let Some(d) = e.max_age_days
                && changes_alone
                && now - updated_at > d * 86_400_000
            {
                add(Issue::Stale, format!("unchanged for over {d} days"));
            }
            if !decoy && !read.contains(&(e.slug.clone(), path.clone(), key.clone())) && now - updated_at > UNREAD_DAYS * 86_400_000 {
                add(Issue::Unread, format!("not read in {UNREAD_DAYS} days"));
            }
        }
        // A public certificate linked into a protected environment and, with its private key,
        // into one that isn't: every key and environment gets the same certificate, so the
        // protected one's key is readable there. Once per protected environment, on its key.
        let mut held: BTreeMap<String, BTreeMap<&str, (&str, &str, bool)>> = BTreeMap::new();
        let mut keyed: BTreeMap<String, BTreeSet<(&str, &str)>> = BTreeMap::new();
        for (env_id, path, key, _, link, ..) in &rows {
            let (Some(e), Some(l)) = (slug.get(env_id.as_str()), link.as_deref().and_then(|l| serde_json::from_str::<crate::Link>(l).ok())) else { continue };
            if l.kind != crate::public_certs::KIND {
                continue;
            }
            let private = l.field == "private_key";
            if e.protected {
                let at = held.entry(l.r#ref.clone()).or_default().entry(e.slug.as_str()).or_insert((path.as_str(), key.as_str(), private));
                if private && !at.2 {
                    *at = (path.as_str(), key.as_str(), true);
                }
            } else if private {
                keyed.entry(l.r#ref).or_default().insert((e.slug.as_str(), path.as_str()));
            }
        }
        if !held.is_empty() && !keyed.is_empty() {
            let names: HashMap<String, String> = self.public_cert_refs(project)?.into_iter().collect();
            for (r, protected) in &held {
                let Some(others) = keyed.get(r) else { continue };
                let named: Vec<&str> = others.iter().filter(|(o, at)| caller.allows(Action::Read, &Scope::at(project, o, at))).map(|(o, _)| *o).collect::<BTreeSet<_>>().into_iter().collect();
                for (env, (path, key, _)) in protected {
                    if named.is_empty() || !caller.allows(Action::Read, &Scope::at(project, env, path)) {
                        continue;
                    }
                    let name = names.get(r).map_or(r.as_str(), String::as_str);
                    let detail = format!("public certificate {name}'s private key is also linked into {}: {env}'s key is readable there (give it a public certificate of its own)", named.join(", "));
                    out.push(Finding { issue: Issue::Exposed, environment: env.to_string(), path: path.to_string(), key: key.to_string(), detail, owner: None, decoy: None });
                }
            }
        }
        // Bindings from before a placement was required.
        let unplaced: Vec<(String, String, String, String)> = self.db.read(|c| c.query("SELECT s.env_id, s.path, s.key, h.binding FROM key_hosts h JOIN secrets s ON s.id = h.secret_id WHERE h.project_id = ? ORDER BY s.path, s.key", [project], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))))?;
        for (env_id, path, key, binding) in unplaced {
            let Some(e) = slug.get(env_id.as_str()) else { continue };
            let placed = serde_json::from_str::<crate::placeholders::Hosts>(&binding).map_or(true, |b| b.placed());
            if !placed && caller.allows(Action::Read, &Scope::at(project, &e.slug, &path)) {
                let detail = format!("a placeholder for {key} may ride in any header, one its host might echo, and an untrusted consumer's doesn't swap: bind it again with a header (Authorization: Bearer {{}}) or a query parameter");
                out.push(Finding { issue: Issue::Unplaced, environment: e.slug.clone(), path, key, detail, owner: None, decoy: None });
            }
        }
        // Required keys a standard environment doesn't define (its imports and base included): each
        // reported only in environments the caller may read.
        for r in self.db.read(|c| crate::required::of(c, project))? {
            for e in &envs {
                if have.contains(&(e.slug.clone(), r.path.clone(), r.key.clone())) || !caller.allows(Action::Read, &Scope::at(project, &e.slug, &r.path)) {
                    continue;
                }
                let seen = self.view(project, &e.slug, &r.path, caller, false, Action::Read)?.iter().any(|x| x.row.key == r.key);
                if !seen {
                    out.push(Finding { issue: Issue::Missing, environment: e.slug.clone(), path: r.path.clone(), key: r.key.clone(), detail: "required".into(), owner: None, decoy: None });
                }
            }
        }
        if code && let Some(refs) = self.code_refs.get() {
            let repos: Vec<String> = self.workspace.get(roll_workspace::Scope::All, project).map(|p| p.resources.into_iter().filter(|r| r.kind == roll_workspace::Kind::Repo).map(|r| r.r#ref).collect()).unwrap_or_default();
            // Whether a name appears in a repository says something about its contents: only for
            // a caller who may read every one of them.
            // Never naming it: that would tell a caller who can't see the canvas that it's there.
            if repos.iter().any(|r| !caller.grants.reads_code(project, r)) {
                return Err(crate::Error::Forbidden("checking the code needs read on every repository on this project's canvas, and you can't read one of them".into()));
            }
            let keys: Vec<String> = have.iter().map(|(_, _, k)| k.clone()).collect::<BTreeSet<_>>().into_iter().take(CODE_KEYS_MAX).collect();
            // Every repository answers, or nothing is said: an unsearchable one might read them all.
            let found: Option<Vec<BTreeSet<String>>> = if repos.is_empty() { None } else { repos.iter().map(|r| refs.referenced(r, &keys)).collect() };
            if let Some(found) = found {
                let read: BTreeSet<&String> = found.iter().flatten().collect();
                let unread_by_code: BTreeSet<&String> = keys.iter().filter(|k| !read.contains(k)).collect();
                for (env_id, path, key, ..) in &rows {
                    let Some(e) = slug.get(env_id.as_str()) else { continue };
                    if unread_by_code.contains(key) && caller.allows(Action::Read, &Scope::at(project, &e.slug, path)) {
                        out.push(Finding { issue: Issue::Unreferenced, environment: e.slug.clone(), path: path.clone(), key: key.clone(), detail: format!("not in {}", repos.join(", ")), owner: None, decoy: None });
                    }
                }
            }
        }
        // Certificates from CSRs that end within two weeks: nothing renews them but whoever asked.
        let ending: Vec<(String, String, String, i64)> = self.db.read(|c| {
            c.query(
                "SELECT a.name, k.names, k.serial, k.not_after FROM ca_certs k JOIN cas a ON a.id = k.ca_id WHERE k.project_id = ? AND k.source = 'csr' AND k.revoked_at IS NULL AND k.not_after >= ? AND k.not_after < ? ORDER BY k.not_after LIMIT 200",
                params![project, now, now + EXPIRING_DAYS * 86_400_000],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
        })?;
        for (ca, names, serial, end) in ending {
            let names = from_json::<Vec<String>>(&names).join(", ");
            out.push(Finding { issue: Issue::CertificateExpiring, environment: String::new(), path: ca, key: names, detail: format!("ends {} (serial {serial}): sign a new CSR", chrono_day(end)), owner: None, decoy: None });
        }
        // SaaS rotators whose own credential ends: after that roll can't issue or revoke their keys.
        for (name, vendor, end) in self.saas_credentials_ending(project, now + EXPIRING_DAYS * 86_400_000)? {
            let (issue, detail) = if end <= now {
                (Issue::Expired, format!("its {vendor} credential expired {}: roll can't issue or revoke its keys", chrono_day(end)))
            } else {
                (Issue::Expiring, format!("its {vendor} credential expires {}: after that roll can't issue or revoke its keys", chrono_day(end)))
            };
            out.push(Finding { issue, environment: String::new(), path: "/".into(), key: name, detail, owner: None, decoy: None });
        }
        // What roll couldn't do for a key: rotate it on schedule, or revoke a credential it issued.
        type Failed = Vec<(String, String, String, String)>;
        let (failing, unrevoked): (Failed, Failed) = self.db.read(|c| {
            Ok::<_, crate::Error>((
                c.query("SELECT env_id, path, key, rotation_error FROM secrets WHERE project_id = ? AND owner = '' AND rotation_error IS NOT NULL", [project], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?,
                // A retired credential's lease names no key: its link's field says what it was.
                c.query(
                    "SELECT env_id, COALESCE(path, '/'), key, link, revoke_error FROM leases WHERE project_id = ? AND revoked_at IS NULL AND revoke_error IS NOT NULL",
                    [project],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?)),
                )?
                .into_iter()
                .map(|(env, path, key, link, why)| {
                    let key = if key.is_empty() { serde_json::from_str::<crate::Link>(&link).map(|l| format!("{} {} ({})", l.kind, l.r#ref, l.field)).unwrap_or_default() } else { key };
                    (env, path, key, why)
                })
                .collect(),
            ))
        })?;
        for (issue, rows) in [(Issue::RotationFailing, failing), (Issue::RevokeFailing, unrevoked)] {
            for (env_id, path, key, why) in rows {
                let Some(e) = slug.get(env_id.as_str()) else { continue };
                if caller.allows(Action::Read, &Scope::at(project, &e.slug, &path)) {
                    out.push(Finding { issue, environment: e.slug.clone(), path, key, detail: why, owner: None, decoy: None });
                }
            }
        }
        // A public certificate's revocation the CA hasn't taken (a leak's, or asked by hand): the
        // job keeps asking. On the registration (`key` its name, `environment` empty).
        if caller.allows(Action::Read, &Scope::whole(project)) {
            let pending: Vec<(String, String, String)> = self.db.read(|c| {
                c.query(
                    "SELECT p.name, r.serial, r.last_error FROM public_cert_revocations r JOIN public_certs p ON p.id = r.cert_id WHERE r.project_id = ? AND r.revoked_at IS NULL AND r.last_error IS NOT NULL AND r.not_after > ? ORDER BY p.name, r.serial",
                    params![project, now],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
            })?;
            for (name, serial, why) in pending {
                out.push(Finding { issue: Issue::RevokeFailing, environment: String::new(), path: String::new(), key: name, detail: format!("certificate {serial} isn't revoked at the CA yet (roll keeps trying): {why}"), owner: None, decoy: None });
            }
        }
        // Syncs and identities made as the operator account: an admin's session before it recorded
        // the person, or the deployment's own token. Nothing tells which, so both are flagged.
        type Made = Vec<(String, String, String, String)>;
        let (syncs, identities): (Made, Made) = self.db.read(|c| {
            Ok::<_, crate::Error>((
                c.query("SELECT e.slug, s.path, s.id, s.kind FROM syncs s JOIN environments e ON e.id = s.env_id AND e.forgotten_at IS NULL WHERE s.project_id = ? AND s.created_by = 'operator' ORDER BY s.id", [project], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?,
                c.query("SELECT '', '/', id, name FROM identities WHERE project_id = ? AND created_by = 'operator' ORDER BY id", [project], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?,
            ))
        })?;
        for (env, path, id, kind) in syncs {
            if caller.allows(Action::Read, &Scope::at(project, &env, &path)) {
                out.push(Finding { issue: Issue::OperatorMade, environment: env, path, key: id, detail: format!("{kind} sync made by the operator account: re-save it (make it again) from a person's session to bind it to them"), owner: None, decoy: None });
            }
        }
        // Syncs that keep a cloud key where they could sign in without one.
        let all: Vec<(String, String, String, String, String)> = self.db.read(|c| {
            c.query("SELECT e.slug, s.path, s.id, s.kind, s.target FROM syncs s JOIN environments e ON e.id = s.env_id AND e.forgotten_at IS NULL WHERE s.project_id = ? ORDER BY s.id", [project], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
        })?;
        // (Under `no_stored_cloud_keys` they're `off_policy` instead.)
        for (env, path, id, kind, target) in all.into_iter().filter(|_| !floors.no_stored_cloud_keys) {
            if !caller.allows(Action::Read, &Scope::at(project, &env, &path)) {
                continue;
            }
            let Ok(t) = self.syncs.get(&kind) else { continue };
            let target: serde_json::Value = serde_json::from_str(&target).unwrap_or_default();
            let Some(instead) = t.stored_cloud_key(&target) else { continue };
            let keys = t.credentials(&target).join(", ");
            let detail = format!("{kind} sync signs in with a cloud key stored in roll ({keys}): {instead}, and delete the key (\"Syncing without a cloud key\")");
            out.push(Finding { issue: Issue::StoredCloudKey, environment: env, path, key: id, detail, owner: None, decoy: None });
        }
        if caller.allows(Action::Read, &Scope::whole(project)) {
            for (env, path, id, name) in identities {
                out.push(Finding { issue: Issue::OperatorMade, environment: env, path, key: id, detail: format!("identity {name} made by the operator account: make it again (or set its pinned keys again) from a person's session to bind it to them"), owner: None, decoy: None });
            }
        }
        // Who to ask: the required key's owner, its folder's rule first, else the key's anywhere.
        let owners: Vec<crate::required::Required> = self.db.read(|c| crate::required::of(c, project))?.into_iter().filter(|r| r.owner.is_some()).collect();
        for f in &mut out {
            f.owner = owners.iter().find(|r| r.key == f.key && r.path == f.path).or_else(|| owners.iter().find(|r| r.key == f.key)).and_then(|r| r.owner.clone());
        }
        let position: HashMap<&str, i64> = envs.iter().map(|e| (e.slug.as_str(), e.position)).collect();
        out.sort_by(|a, b| (a.issue, position.get(a.environment.as_str()), &a.path, &a.key).cmp(&(b.issue, position.get(b.environment.as_str()), &b.path, &b.key)));
        Ok(out)
    }
}

/// The most names one kind lists in a `production_unprotected` finding.
const REACH_NAMES: usize = 10;

/// `names` joined, the first [`REACH_NAMES`] and how many more.
fn some(names: &BTreeSet<String>) -> String {
    let shown: Vec<&str> = names.iter().take(REACH_NAMES).map(String::as_str).collect();
    match names.len().saturating_sub(REACH_NAMES) {
        0 => shown.join(", "),
        n => format!("{} and {n} more", shown.join(", ")),
    }
}

    /// How far `project` is from roll secrets' policy, as `caller` may read it (`off_policy`
    /// findings): each project's Health and the operator's report across projects
    /// ([`Secrets::policy_report`]) both come from here. `envs`: the project's standard ones.
    pub(crate) fn off_policy(&self, project: &str, caller: &Caller, floors: &crate::guardrails::Guardrails, envs: &[envs::EnvRow]) -> Result<Vec<Finding>> {
        let mut out = vec![];
        if let Some(n) = floors.production_approvals
            && let Some(e) = envs.iter().find(|e| e.slug == crate::PRODUCTION && e.kind == Kind::Standard)
            && (!e.protected || e.approvals < n)
            && caller.allows(Action::Read, &Scope::env(project, &e.slug))
        {
            let has = if e.protected { format!("{} approval{}", e.approvals, if e.approvals == 1 { "" } else { "s" }) } else { "no protection".into() };
            let detail = format!("roll secrets' policy keeps production protected with at least {n} approval{}; it has {has}", if n == 1 { "" } else { "s" });
            out.push(Finding { issue: Issue::OffPolicy, environment: e.slug.clone(), path: "/".into(), key: String::new(), detail, owner: None, decoy: None });
        }
        if floors.leaks_rotate == Some(crate::leaks::LeakRotate::Managed) && self.leak_rotate(project)? != crate::leaks::LeakRotate::Managed && caller.allows(Action::Read, &Scope::whole(project)) {
            let detail = "roll secrets' policy keeps leaks.rotate managed; it's off, so a leaked managed key isn't rotated".to_string();
            out.push(Finding { issue: Issue::OffPolicy, environment: String::new(), path: "/".into(), key: String::new(), detail, owner: None, decoy: None });
        }
        if floors.no_breakglass && caller.allows(Action::Read, &Scope::whole(project)) && !self.breakglass(project)?.is_empty() {
            let detail = "roll secrets' policy allows no break glass; this project's breakglass list names someone (it's refused at the hook either way)".to_string();
            out.push(Finding { issue: Issue::OffPolicy, environment: String::new(), path: "/".into(), key: String::new(), detail, owner: None, decoy: None });
        }
        let syncs: Vec<(String, String, String, String, String)> = self.db.read(|c| {
            c.query("SELECT e.slug, s.path, s.id, s.kind, s.target FROM syncs s JOIN environments e ON e.id = s.env_id AND e.forgotten_at IS NULL WHERE s.project_id = ? ORDER BY s.id", [project], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
        })?;
        for (env, path, id, kind, target) in syncs {
            if !caller.allows(Action::Read, &Scope::at(project, &env, &path)) {
                continue;
            }
            let Ok(t) = self.syncs.get(&kind) else { continue };
            if let Some(kinds) = floors.sync_kinds.as_ref().filter(|ks| !t.internal() && !ks.contains(&kind)) {
                let allowed = if kinds.is_empty() { "none".to_string() } else { kinds.join(", ") };
                let detail = format!("{kind} sync: roll secrets' policy allows only these sync kinds ({allowed})");
                out.push(Finding { issue: Issue::OffPolicy, environment: env.clone(), path: path.clone(), key: id.clone(), detail, owner: None, decoy: None });
            }
            let target: serde_json::Value = serde_json::from_str(&target).unwrap_or_default();
            if let (true, Some(instead)) = (floors.no_stored_cloud_keys, t.stored_cloud_key(&target)) {
                let keys = t.credentials(&target).join(", ");
                let detail = format!("{kind} sync signs in with a cloud key stored in roll ({keys}), which roll secrets' policy rules out: {instead}, and delete the key (\"Syncing without a cloud key\")");
                out.push(Finding { issue: Issue::OffPolicy, environment: env, path, key: id, detail, owner: None, decoy: None });
            }
        }
        Ok(out)
    }

impl Secrets {
    /// What reaches an unprotected `production`, as a finding's detail: members by role (owners and
    /// admins everywhere, developers who name it), and the checks and sandboxes that read it in the
    /// last 90 days (its audit log: what can be told without reading any repository).
    fn production_reach(&self, project: &str, now: i64) -> Result<String> {
        use roll_auth::roles::Role;
        let mut by_role: [(Role, &str, BTreeSet<String>); 3] = [(Role::Owner, "owners", BTreeSet::new()), (Role::Admin, "admins", BTreeSet::new()), (Role::Developer, "developers who name it", BTreeSet::new())];
        for m in self.workspace.members(roll_workspace::Scope::All, project, false).unwrap_or_default() {
            let names_it = m.environments.as_ref().is_some_and(|l| l.iter().any(|e| roll_auth::scope::env_covers(e, crate::PRODUCTION)));
            if m.inactive || (m.role == Role::Developer && !names_it) {
                continue;
            }
            if let Some((_, _, set)) = by_role.iter_mut().find(|(r, ..)| *r == m.role) {
                set.insert(m.handle);
            }
        }
        let rows: Vec<(String, String)> = self.db.read(|c| {
            c.query(
                "SELECT actor, summary FROM audit WHERE project_id = ? AND env = ? AND action = 'secrets.resolve' AND at >= ? ORDER BY id DESC LIMIT 2000",
                params![project, crate::PRODUCTION, now - UNREAD_DAYS * 86_400_000],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })?;
        let (mut checks, mut sandboxes) = (BTreeSet::new(), BTreeSet::new());
        for (actor, summary) in rows {
            if let Some(check) = actor.strip_prefix("check:") {
                checks.insert(check.to_string());
            } else if let Some(rest) = summary.split(" for sandbox:").nth(1) {
                sandboxes.insert(rest.split(' ').next().unwrap_or_default().to_string());
            }
        }
        let mut reach: Vec<String> = by_role.iter().filter(|(.., s)| !s.is_empty()).map(|(_, name, s)| format!("{name} {}", some(s))).collect();
        if !checks.is_empty() {
            reach.push(format!("checks {}", some(&checks)));
        }
        if !sandboxes.is_empty() {
            reach.push(format!("sandboxes {}", some(&sandboxes)));
        }
        let reach = if reach.is_empty() { String::new() } else { format!("; these reach it: {}", reach.join("; ")) };
        Ok(format!("production isn't protected: any check or sandbox may read it, untrusted previews too, and no change there needs approval{reach}"))
    }
}

/// 2027-03-31 (UTC).
impl Secrets {
    /// [`Issue::BackupLeft`]: one finding per person who took backups carrying registrations' keys
    /// in the project's history and may no longer back it up, as they are now.
    fn backups_left(&self, project: &str, now: i64) -> Result<Vec<Finding>> {
        let since = now - crate::backup::taken_days(self.history_days(project)?) * 86_400_000;
        let mut by: BTreeMap<String, (i64, BTreeSet<String>)> = BTreeMap::new();
        for t in self.backups_taken(project, since, false)?.into_iter().filter(|t| !t.registrations.is_empty()) {
            if let Some(user) = self.reader_person(&t.actor) {
                let e = by.entry(user).or_default();
                e.0 = e.0.max(t.at);
                e.1.extend(t.registrations);
            }
        }
        let mut out = vec![];
        for (user, (last, carried)) in by {
            if self.maker_reaches(&user, &[(Action::Admin, Scope::whole(project)), (Action::Reveal, Scope::whole(project))])? {
                continue;
            }
            let advice: Vec<String> = carried.iter().map(|r| crate::backup::advice(r)).collect();
            let detail = format!("took a backup on {} and may no longer back the project up: {}", chrono_day(last), advice.join("; "));
            out.push(Finding { issue: Issue::BackupLeft, environment: String::new(), path: "/".into(), key: user, detail, owner: None, decoy: None });
        }
        Ok(out)
    }
}

pub(crate) fn chrono_day(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    #[test]
    fn days_are_civil_dates() {
        assert_eq!(super::chrono_day(0), "1970-01-01");
        assert_eq!(super::chrono_day(1_869_609_600_000), "2029-03-31");
        assert_eq!(super::chrono_day(951_782_400_000), "2000-02-29");
    }
}

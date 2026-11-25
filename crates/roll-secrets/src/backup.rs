//! Encrypted backup and restore (docs/secrets.md, "Backup and restore"): a project's environments,
//! folders, own keys, imports, policies, required keys, encryption keys, the registrations its
//! links name (CAs' and SSH CAs' private keys among them; `backup_rows.rs`), its identities, syncs
//! and settings as one archive, sealed to a key only the caller holds (`roll-seal-v1`), and loaded
//! back into an empty project, on this server or another.
//!
//! Linked keys carry their link and no value: a restore issues its own credentials, from the
//! restored registrations. Personal values, history, leases and tokens aren't in it. `verify`
//! compares an opened archive with the live project and writes nothing: the restore drill.

use crate::access::{Action, Caller, Scope};
use crate::backup_rows::{self, IDENTITIES, REGISTRATIONS, Row, SYNCS, Table};
use crate::envs::{self, Kind};
use crate::{Error, Import, Link, Op, Required, Result, Secrets, Value, audit, from_json, names};
use code_sql::{OptionalExtension, Value as Cell, params};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use std::collections::BTreeMap;

/// The archive's version: 2 carries registrations, identities, syncs and settings. A restore
/// reads 1 too.
const V: u8 = 2;

/// Why a restored identity or sync waits: it acts as its maker, and a backup carries none.
const RESTORED: &str = "restored from a backup: an admin resumes it, and acts as its maker from then on";

#[derive(Debug, Serialize, Deserialize)]
pub struct Archive {
    pub v: u8,
    /// The project it was taken from (a restore may load it into another).
    pub project: String,
    pub at: i64,
    pub environments: Vec<ArchivedEnvironment>,
    #[serde(default)]
    pub required: Vec<Required>,
    /// Encryption keys (transit.rs): their bytes, so a restore still opens what they encrypted.
    #[serde(default)]
    pub encryption_keys: Vec<crate::transit::ArchivedKey>,
    /// The registrations links name, with their keys and credentials (v2): CAs, SSH CAs, SaaS
    /// rotators, public certificates, rotators, databases outside roll, GitHub Apps.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub registrations: Vec<Row>,
    /// The project's ACME accounts, by directory (v2).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acme_accounts: Vec<AcmeAccount>,
    /// Restored disabled, with no maker (v2).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub identities: Vec<Row>,
    /// Restored paused, with no maker (v2).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub syncs: Vec<Row>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<Settings>,
    /// The registrations the project had that a backup doesn't carry, by name ("AWS role deploy").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub left_out: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AcmeAccount {
    pub directory: String,
    pub credentials: Value,
}

/// The project's own settings (`PATCH …/settings`).
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    pub history_days: Option<i64>,
    #[serde(default)]
    pub validity_checks: bool,
    /// `leaks.rotate`: `managed` or `off`.
    pub leak_rotate: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ArchivedEnvironment {
    pub slug: String,
    pub name: String,
    pub kind: Kind,
    pub base: Option<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub trusted: bool,
    /// An ephemeral environment's branched resources: restored with it, or it isn't restored.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub branches: Vec<crate::Branch>,
    #[serde(default)]
    pub protected: bool,
    #[serde(default)]
    pub approvals: i64,
    #[serde(default)]
    pub approvers: Vec<String>,
    pub max_age_days: Option<i64>,
    #[serde(default)]
    pub folders: Vec<String>,
    /// Folders' own change policies: (path, approvals, approvers).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub folder_policies: Vec<(String, i64, Vec<String>)>,
    /// By folder.
    #[serde(default)]
    pub imports: BTreeMap<String, Vec<Import>>,
    #[serde(default)]
    pub secrets: Vec<ArchivedSecret>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ArchivedSecret {
    pub path: String,
    pub key: String,
    /// None for a linked key (its link is restored and issues its own credential).
    pub value: Option<Value>,
    #[serde(default)]
    pub comment: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub link: Option<Link>,
    #[serde(default)]
    pub expires_at: Option<i64>,
}

/// What a restore did.
#[derive(Debug, Serialize)]
pub struct Restored {
    pub environments: usize,
    pub secrets: usize,
    pub registrations: usize,
    /// Restored disabled: an admin enables each.
    pub identities: usize,
    /// Restored paused: an admin resumes each.
    pub syncs: usize,
    /// What couldn't be restored and why (an import of another project's folder, a link to a
    /// resource this project doesn't have): names, never values.
    pub warnings: Vec<String>,
}

/// One part of an archive against the live project, by name: never a value. A token's answer
/// has counts only: its lists are empty ([`Verified::names`]).
#[derive(Debug, Default, Serialize)]
pub struct Diff {
    pub archive: usize,
    pub live: usize,
    /// In the archive, not in the project.
    pub missing: Vec<String>,
    /// In the project, not in the archive.
    pub extra: Vec<String>,
    /// In both, different: another value, link, key or certificate.
    pub changed: Vec<String>,
    /// How many are missing, extra and changed: the lists' lengths, or a token's whole answer.
    pub counts: Counts,
}

#[derive(Debug, Default, Serialize)]
pub struct Counts {
    pub missing: usize,
    pub extra: usize,
    pub changed: usize,
}

/// A sealed backup, and what it left out for this caller (a token's carries no registration's key).
#[derive(Debug)]
pub struct Backup {
    pub sealed: crate::crypto::Sealed,
    pub warnings: Vec<String>,
}

/// What `verify` found (`POST …/backup/verify`).
#[derive(Debug, Serialize)]
pub struct Verified {
    /// The project the backup was taken from, when, and its version.
    pub project: String,
    pub at: i64,
    pub v: u8,
    pub environments: Diff,
    pub keys: Diff,
    pub registrations: Diff,
    pub identities: Diff,
    pub syncs: Diff,
    /// What a restore would leave out, and why: a registration or an identity that fails the
    /// checks making it runs (names, never values). Empty for a token's answer.
    pub refused: Vec<String>,
    /// Whether the parts name what differs: only for a person or the operator. A token gets counts.
    pub names: bool,
    /// The settings that differ (`history_days`, `validity_checks`, `leaks.rotate`).
    pub settings: Vec<String>,
    /// The registrations the archive says it doesn't carry.
    pub left_out: Vec<String>,
    /// The keys the project has now: a restore goes only into a project with none.
    pub live_keys: usize,
}

/// A backup and a verify reach every value of the project, protected environments' and the
/// registrations' keys included: only a caller who may manage and reveal all of it, as a schedule's
/// maker must (`backup_schedule.rs`). `admin` alone never includes `reveal`.
pub(crate) fn need_whole(caller: &Caller, project: &str) -> Result<()> {
    caller.need(Action::Admin, &Scope::whole(project))?;
    caller.need(Action::Reveal, &Scope::whole(project))
}

/// Whether registrations' keys and credentials (CAs', SSH CAs' and GitHub Apps' private keys, SaaS
/// and DNS credentials, ACME accounts, rotators' and outside databases' URLs) may leave in this
/// caller's backup, and per-key answers come back from its verify: a person (their session) or the
/// operator, never a token, which no route answers them to either.
pub(crate) fn carries_keys(caller: &Caller) -> bool {
    caller.principal == "operator" || crate::access::is_person(&caller.principal)
}

/// The most verifies a caller makes a minute: each reads every value of the project.
pub const VERIFIES_PER_MINUTE: i64 = 10;

/// Why a token's backup holds a registration by name only.
const NO_KEYS: &str = "a token's backup carries no registration's key: back it up signed in, or as the operator, to keep it";

impl Secrets {
    /// The project's archive sealed to `recipient` (an X25519 public key). Every value of the
    /// project leaves in it, and its registrations' keys, so it's for a caller who may manage and
    /// reveal all of it ([`need_whole`]), and audited.
    /// A token's carries the project's registrations by name only ([`carries_keys`]).
    pub fn backup(&self, project: &str, recipient: &str, caller: &Caller) -> Result<Backup> {
        need_whole(caller, project)?;
        self.backup_as(project, recipient, caller, carries_keys(caller))
    }

    /// [`Secrets::backup`], checked: `keys` says whether registrations leave with their keys. The
    /// audit row names the registrations that did (its `keys`), for reach and offboarding.
    pub(crate) fn backup_as(&self, project: &str, recipient: &str, caller: &Caller, keys: bool) -> Result<Backup> {
        let mut archive = self.archive(project)?;
        let mut warnings = vec![];
        if !keys && !(archive.registrations.is_empty() && archive.acme_accounts.is_empty()) {
            let mut held: Vec<String> = archive.registrations.drain(..).map(|r| registration_label(&r)).collect();
            held.extend(archive.acme_accounts.drain(..).map(|a| format!("ACME account at {}", a.directory)));
            warnings.push(format!("left out {}: {NO_KEYS}", held.join(", ")));
            archive.left_out.extend(held.into_iter().map(|h| format!("{h} ({NO_KEYS})")));
        }
        let plain = zeroize::Zeroizing::new(serde_json::to_vec(&archive).expect("JSON"));
        let sealed = crate::crypto::seal_to(recipient, &context(project), &plain).map_err(Error::BadRequest)?;
        let n: usize = archive.environments.iter().map(|e| e.secrets.len()).sum();
        let carried: Vec<String> = archive.registrations.iter().map(registration_label).chain(archive.acme_accounts.iter().map(|a| format!("ACME account at {}", a.directory))).collect();
        let summary = format!(
            "backed up {} environments, {} keys, {} registrations{}, {} identities and {} syncs, sealed to a key of the caller's",
            archive.environments.len(),
            n,
            archive.registrations.len(),
            if keys { " with their keys" } else { " (by name only: a token's)" },
            archive.identities.len(),
            archive.syncs.len()
        );
        self.audit_now(project, caller, "project.backup", None, None, &carried, &summary);
        Ok(Backup { sealed, warnings })
    }

    /// The project as an archive, every value and registration key opened (it's sealed or
    /// compared at once, never kept).
    fn archive(&self, project: &str) -> Result<Archive> {
        let dek = self.project(project)?;
        let now = self.db.now_ms()?;
        type Row = (String, String, i64, Option<Vec<u8>>, String, String, String, Option<String>, Option<i64>);
        let archive = self.db.read(|c| {
            let rows = envs::all(c, project)?;
            let slugs: BTreeMap<String, String> = rows.iter().map(|e| (e.id.clone(), e.slug.clone())).collect();
            let mut environments = vec![];
            for e in &rows {
                let folders: Vec<String> = c.query("SELECT path FROM folders WHERE env_id = ? ORDER BY path", [&e.id], |r| r.get(0))?;
                let mut imports = BTreeMap::new();
                for f in &folders {
                    let list = envs::imports_of(c, &e.id, f)?;
                    if !list.is_empty() {
                        imports.insert(f.clone(), list);
                    }
                }
                let secrets: Vec<Row> = c.query(
                    "SELECT id, path, version, value, key, comment, tags, link, expires_at FROM secrets WHERE env_id = ? AND owner = '' ORDER BY path, key",
                    [&e.id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?)),
                )?;
                let mut out = vec![];
                for (id, path, version, ct, key, comment, tags, link, expires_at) in secrets {
                    let link: Option<Link> = link.and_then(|l| serde_json::from_str(&l).ok());
                    // A sensitive key's value never leaves roll for a person, a backup included:
                    // it's restored by name, to be set again.
                    let sensitive = crate::names::sensitive(&from_json::<Vec<String>>(&tags));
                    let value = match (&link, ct) {
                        _ if sensitive => None,
                        (None, Some(ct)) => Some(dek.open(&crate::crypto::value_aad(project, &id, version), &ct)?),
                        _ => None,
                    };
                    out.push(ArchivedSecret { path, key, value, comment, tags: from_json(&tags), link, expires_at });
                }
                environments.push(ArchivedEnvironment {
                    slug: e.slug.clone(),
                    name: e.name.clone(),
                    kind: e.kind,
                    base: e.base_id.as_ref().and_then(|b| slugs.get(b).cloned()),
                    labels: e.labels.clone(),
                    expires_at: e.expires_at,
                    trusted: e.trusted,
                    branches: e.branches.clone(),
                    protected: e.protected,
                    approvals: e.approvals,
                    approvers: e.approvers.clone(),
                    max_age_days: e.max_age_days,
                    folders,
                    folder_policies: self.db.read(|c| crate::folder_policy::all(c, &e.id))?.into_iter().map(|p| (p.path, p.approvals, p.approvers)).collect(),
                    imports,
                    secrets: out,
                });
            }
            let mut registrations = vec![];
            for t in REGISTRATIONS {
                registrations.extend(backup_rows::export(c, t, project, &dek, now)?);
            }
            let accounts: Vec<(String, Vec<u8>)> = c.query("SELECT directory, credentials FROM public_cert_accounts WHERE project_id = ? ORDER BY directory", [project], |r| Ok((r.get(0)?, r.get(1)?)))?;
            let mut acme_accounts = vec![];
            for (directory, ct) in accounts {
                acme_accounts.push(AcmeAccount { credentials: dek.open(&crate::public_certs::account_aad(project, &directory), &ct)?, directory });
            }
            let mut syncs = backup_rows::export(c, &SYNCS, project, &dek, now)?;
            for s in &mut syncs {
                let env = s.columns.remove("env_id").and_then(|id| id.as_str().and_then(|id| slugs.get(id)).cloned()).unwrap_or_default();
                s.columns.insert("environment".into(), Json::String(env));
            }
            let settings = c.query_row("SELECT history_days, validity_checks, leak_rotate FROM projects WHERE project_id = ?", [project], |r| {
                Ok(Settings { history_days: r.get(0)?, validity_checks: r.get::<_, Option<i64>>(1)? == Some(1), leak_rotate: r.get(2)? })
            })?;
            Ok::<_, Error>(Archive {
                v: V,
                project: project.to_string(),
                at: now,
                environments,
                required: crate::required::of(c, project)?,
                encryption_keys: vec![],
                registrations,
                acme_accounts,
                identities: backup_rows::export(c, &IDENTITIES, project, &dek, now)?,
                syncs,
                settings: Some(settings),
                left_out: backup_rows::left_out(c, project)?,
            })
        })?;
        let mut archive = archive;
        archive.encryption_keys = self.transit_export(project)?;
        Ok(archive)
    }

    /// Load `archive` into `project`, which has no secrets yet. Registrations come first (links
    /// name them), made by the caller; values are written before any protection is set (a restore
    /// isn't a change request); identities and syncs come last, waiting for an admin. What can't
    /// be restored is a warning.
    pub fn restore(&self, project: &str, archive: Archive, caller: &Caller) -> Result<Restored> {
        caller.need(Action::Admin, &Scope::project(project))?;
        if !(1..=V).contains(&archive.v) {
            return Err(Error::BadRequest(format!("an archive of version {} (this server reads 1 to {V})", archive.v)));
        }
        let dek = self.project(project)?;
        let used: i64 = self.db.read(|c| Ok::<_, Error>(c.query_row("SELECT COUNT(*) FROM secrets WHERE project_id = ?", [project], |r| r.get(0))?))?;
        if used > 0 {
            return Err(Error::Conflict("conflict", "a restore goes into a project with no secrets yet".into()));
        }
        let mut warnings = vec![];
        let (moved, registrations) = self.restore_registrations(project, &archive, &dek, caller, &mut warnings)?;
        let (mut environments, mut secrets) = (0, 0);
        // Standard environments first (bases), then ephemeral ones; each as it was.
        let mut order: Vec<&ArchivedEnvironment> = archive.environments.iter().filter(|e| e.kind == Kind::Standard).collect();
        order.extend(archive.environments.iter().filter(|e| e.kind == Kind::Ephemeral));
        for e in &order {
            let ensure = crate::Ensure { name: Some(e.name.clone()), kind: Some(e.kind), base: e.base.clone(), labels: Some(e.labels.clone()), expires_at: e.expires_at, trusted: e.trusted, branches: (!e.branches.is_empty()).then(|| e.branches.clone()), ..Default::default() };
            match self.ensure_environment(project, &e.slug, ensure, caller) {
                Ok(_) => environments += 1,
                Err(err) => {
                    warnings.push(format!("{}: {err}", e.slug));
                    continue;
                }
            }
            for f in &e.folders {
                if f != "/" && let Err(err) = self.create_folder(project, &e.slug, f, caller) {
                    warnings.push(format!("{}:{f}: {err}", e.slug));
                }
            }
            let mut by_path: BTreeMap<&str, Vec<Op>> = BTreeMap::new();
            for s in &e.secrets {
                by_path.entry(&s.path).or_default().push(Op::Set {
                    key: s.key.clone(),
                    value: s.value.as_ref().map(|v| v.expose().to_string()),
                    comment: Some(s.comment.clone()),
                    tags: Some(s.tags.clone()),
                    kind: None,
                    // To the restored registration, which has an id of its own.
                    link: s.link.clone().map(|mut l| {
                        if let Some(id) = moved.get(&l.r#ref) {
                            l.r#ref = id.clone();
                        }
                        Some(l)
                    }),
                    path: None,
                    expires_at: s.expires_at.map(Some),
                    if_version: None,
                });
            }
            for (path, ops) in by_path {
                let write = |ops: Vec<Op>| self.batch_via(project, &e.slug, crate::Batch { path: Some(path.to_string()), message: Some("restored from a backup".into()), ops, if_seq: None, confirm: None }, crate::write::Via::Restore, caller);
                let n = ops.len();
                let linked = |o: &Op| matches!(o, Op::Set { link: Some(Some(_)), .. });
                match write(ops.clone()) {
                    Ok(_) => secrets += n,
                    // A link this project can't make (its resource isn't on the canvas) mustn't
                    // cost the folder's plain values: those go in alone, each link is a warning.
                    Err(err) if ops.iter().any(linked) => {
                        let (links, plain): (Vec<Op>, Vec<Op>) = ops.into_iter().partition(linked);
                        let keys: Vec<String> = links.iter().map(|o| match o {
                            Op::Set { key, .. } | Op::Delete { key, .. } => key.clone(),
                        }).collect();
                        warnings.push(format!("{}:{path}: links not restored ({}): {err}", e.slug, keys.join(", ")));
                        if !plain.is_empty() {
                            let m = plain.len();
                            match write(plain) {
                                Ok(_) => secrets += m,
                                Err(err) => warnings.push(format!("{}:{path}: {err}", e.slug)),
                            }
                        }
                    }
                    Err(err) => warnings.push(format!("{}:{path}: {err}", e.slug)),
                }
            }
            if let Some(d) = e.max_age_days
                && let Err(err) = self.update_environment(project, &e.slug, crate::EnvChange { max_age_days: Some(Some(d)), ..Default::default() }, caller)
            {
                warnings.push(format!("{}: {err}", e.slug));
            }
        }
        for e in &order {
            for (path, list) in &e.imports {
                if let Err(err) = self.set_imports(project, &e.slug, path, list.clone(), caller) {
                    warnings.push(format!("{}:{path} imports: {err}", e.slug));
                }
            }
        }
        if !archive.required.is_empty()
            && let Err(err) = self.set_required(project, archive.required, caller)
        {
            warnings.push(format!("required keys: {err}"));
        }
        // Protection last: it would turn the writes above into change requests.
        for e in &order {
            for (path, approvals, approvers) in &e.folder_policies {
                let p = crate::folder_policy::SetFolderPolicy { approvals: Some(*approvals), approvers: Some(approvers.clone()) };
                if let Err(err) = self.set_folder_policy(project, &e.slug, path, p, caller) {
                    warnings.push(format!("{} {path} policy: {err}", e.slug));
                }
            }
        }
        // A production below roll secrets' policy comes back at its floor (guardrails.rs).
        let floor = self.floors()?.production_approvals.unwrap_or(0);
        for e in order.iter().filter(|e| e.protected) {
            let approvals = if e.slug == crate::PRODUCTION { e.approvals.max(floor) } else { e.approvals };
            let p = envs::Policy { protected: true, approvals: Some(approvals), approvers: Some(e.approvers.clone()), notify_url: None };
            if let Err(err) = self.set_policy(project, &e.slug, p, caller) {
                warnings.push(format!("{} policy: {err}", e.slug));
            }
        }
        warnings.extend(self.transit_import(project, archive.encryption_keys, caller)?);
        let (identities, syncs) = self.restore_workloads(project, &archive.identities, &archive.syncs, &dek, caller, &mut warnings)?;
        let now = self.db.now_ms()?;
        let summary = format!(
            "restored {environments} environments, {secrets} keys, {registrations} registrations, {identities} identities (disabled) and {syncs} syncs (paused) from {}'s backup",
            archive.project
        );
        self.db.write(|c| audit::record(c, self.keys()?, project, caller, "project.restore", None, None, &[], &summary, now))?;
        Ok(Restored { environments, secrets, registrations, identities, syncs, warnings })
    }

    /// The archive's registrations, ACME accounts and settings, in one write, made by the caller
    /// (as [`Secrets::maker_of`] records it). Each is checked first as registering it would,
    /// without asking its service (that happens at its first use); one this project already has
    /// by name is kept as it is. Returns the new id of each restored one, by its old id.
    fn restore_registrations(&self, project: &str, archive: &Archive, dek: &crate::crypto::Dek, caller: &Caller, warnings: &mut Vec<String>) -> Result<(BTreeMap<String, String>, usize)> {
        if !archive.left_out.is_empty() {
            warnings.push(format!("not in the backup, registered again by hand: {}", archive.left_out.join(", ")));
        }
        let by = if archive.registrations.is_empty() { String::new() } else { self.maker_of(caller)? };
        let mut admitted: Vec<(&Table, &Row, String, Vec<(&str, Cell)>)> = vec![];
        for r in &archive.registrations {
            let Some(t) = backup_rows::table(&r.kind) else {
                warnings.push(format!("a {} registration isn't one a backup carries: left out", r.kind.chars().take(40).collect::<String>()));
                continue;
            };
            match self.admit(t, r) {
                Ok(checked) => admitted.push((t, r, names::new_id(t.prefix, t.len), checked)),
                Err(e) => warnings.push(format!("{} {}: {e}; not restored", t.what, r.text("name"))),
            }
        }
        let settings = archive.settings.clone().filter(|s| {
            let ok = s.history_days.is_none_or(|d| (1..=3650).contains(&d)) && s.leak_rotate.as_deref().is_none_or(|r| crate::leaks::LeakRotate::parse(r).is_some());
            if !ok {
                warnings.push("the project's settings aren't ones roll takes: not restored".into());
            }
            ok
        });
        if admitted.is_empty() && archive.acme_accounts.is_empty() && settings.is_none() {
            return Ok((BTreeMap::new(), 0));
        }
        let now = self.db.now_ms()?;
        let (moved, kept) = self.db.write(|c| {
            let (mut moved, mut kept) = (BTreeMap::new(), vec![]);
            for (t, r, id, checked) in &admitted {
                let name = r.text("name");
                if c.query_row(&format!("SELECT 1 FROM {} WHERE project_id = ? AND name = ?", t.table), [project, name], |_| Ok(())).optional()?.is_some() {
                    kept.push(format!("{} {name}: this project has one by that name, kept as it is", t.what));
                    continue;
                }
                backup_rows::import(c, t, project, id, &by, now, r, dek, checked)?;
                moved.insert(r.id.clone(), id.clone());
            }
            for a in &archive.acme_accounts {
                if c.query_row("SELECT 1 FROM public_cert_accounts WHERE project_id = ? AND directory = ?", [project, a.directory.as_str()], |_| Ok(())).optional()?.is_none() {
                    let sealed = dek.seal(&crate::public_certs::account_aad(project, &a.directory), &a.credentials);
                    c.execute("INSERT INTO public_cert_accounts (project_id, directory, credentials, created_at) VALUES (?, ?, ?, ?)", params![project, a.directory, sealed, now])?;
                }
            }
            if let Some(s) = &settings {
                // Under roll secrets' policy, a restore comes back at its floors (guardrails.rs).
                let mut leak_rotate = s.leak_rotate.clone();
                if crate::guardrails::read(c)?.leaks_rotate == Some(crate::leaks::LeakRotate::Managed) && leak_rotate.as_deref() != Some("managed") {
                    leak_rotate = Some("managed".into());
                    kept.push("leaks.rotate: roll secrets' policy keeps it managed, so it's restored as managed".into());
                }
                c.execute("UPDATE projects SET history_days = ?, validity_checks = ?, leak_rotate = ? WHERE project_id = ?", params![s.history_days, s.validity_checks as i64, leak_rotate, project])?;
            }
            Ok::<_, Error>((moved, kept))
        })?;
        warnings.extend(kept);
        let n = moved.len();
        Ok((moved, n))
    }

    /// A registration from an archive checked as registering it would be, without its service:
    /// a name roll takes, the columns its kind needs, its URLs and settings within what roll
    /// allows; a CA's or SSH CA's certificate, chain, name constraints and key as installing or
    /// making it checks them. The archive is the caller's own JSON, so nothing in it is taken on
    /// trust. Answers the columns to write as checked (over the archive's). `verify` asks it too.
    fn admit(&self, t: &Table, r: &Row) -> Result<Vec<(&'static str, Cell)>> {
        let bad = |why: &str| Err(Error::BadRequest(why.to_string()));
        let name = r.text("name");
        if name.is_empty() || name.len() > 64 || !name.chars().all(|c| c.is_ascii_alphanumeric() || "-_".contains(c)) {
            return bad("its name isn't one roll takes");
        }
        let needs = |columns: &[&str]| match columns.iter().find(|c| !r.sealed.contains_key(**c)) {
            Some(c) => Err(Error::BadRequest(format!("the backup holds no {c} for it"))),
            None => Ok(()),
        };
        let json = |column: &str| serde_json::from_str::<Json>(r.text(column)).map_err(|_| Error::BadRequest(format!("its {column} isn't JSON")));
        match t.table {
            "cas" => {
                let policy: crate::pki::Policy = serde_json::from_value(json("policy")?).map_err(|_| Error::BadRequest("its policy isn't one roll reads".into()))?;
                needs(&["private_key"])?;
                let unconstrained = r.columns.get("unconstrained").and_then(Json::as_i64).is_some_and(|n| n != 0);
                let text = |c: &str| Some(r.text(c)).filter(|t| !t.is_empty());
                let a = crate::pki::admit_archived(text("certificate"), text("csr"), text("chain"), unconstrained, policy, r.sealed["private_key"].expose())?;
                Ok(vec![
                    ("certificate", Cell::Text(a.certificate)),
                    ("chain", a.chain.map_or(Cell::Null, Cell::Text)),
                    ("policy", Cell::Text(serde_json::to_string(&a.policy).expect("JSON"))),
                    ("unconstrained", Cell::Int(i64::from(a.unconstrained))),
                ])
            }
            "ssh_cas" => {
                needs(&["private_key"])?;
                let policy = crate::ssh::admit_archived(r.text("public_key"), r.text("policy"), r.sealed["private_key"].expose())?;
                Ok(vec![("policy", Cell::Text(serde_json::to_string(&policy).expect("JSON")))])
            }
            "saas_rotators" => {
                crate::saas::Vendor::parse(r.text("vendor"), json("target")?)?.checked()?;
                needs(&["credential"]).map(|_| vec![])
            }
            "public_certs" => {
                if !self.public_ca()?.allows(r.text("directory")) {
                    return bad("its CA isn't one this server orders from (ROLL_SECRETS_ACME_ALLOW)");
                }
                if crate::public_certs::DnsProvider::parse(r.text("dns_provider")).is_none() {
                    return bad("its DNS provider isn't one roll takes");
                }
                needs(&["dns_credential"]).map(|_| vec![])
            }
            "rotators" => {
                needs(&["url", "secret"])?;
                let deliver = self.deliver.as_ref().ok_or_else(|| Error::BadRequest("rotators aren't on this server".into()))?;
                // Its URL may hold a token: refused without saying what it is.
                deliver.check(r.sealed["url"].expose()).map_err(|_| Error::BadRequest("its URL isn't one roll calls".into()))?;
                Ok(vec![])
            }
            "outside_dbs" => {
                if self.outside.is_none() {
                    return bad("databases outside roll aren't on this server");
                }
                if r.columns.get("gateway").is_some_and(|g| !g.is_null()) {
                    return bad("it's reached through a gateway, which a backup doesn't carry: register it again with a new one");
                }
                needs(&["url"])?;
                crate::outside::check_url(r.sealed["url"].expose()).map_err(|_| Error::BadRequest("its URL isn't one roll connects to".into()))?;
                crate::outside::check_ca(r.columns.get("ca").and_then(Json::as_str).map(str::to_string)).map(|_| vec![])
            }
            "github_apps" => {
                let github = self.github.as_ref().ok_or_else(|| Error::BadRequest("GitHub App tokens aren't on this server".into()))?;
                github.check(r.text("api_base")).map_err(Error::BadRequest)?;
                crate::github_app::check_repositories(&serde_json::from_value::<Vec<String>>(json("repositories")?).map_err(|_| Error::BadRequest("its repositories aren't a list".into()))?)?;
                crate::github_app::check_permissions(&serde_json::from_value(json("permissions")?).map_err(|_| Error::BadRequest("its permissions aren't a map".into()))?)?;
                needs(&["private_key"]).map(|_| vec![])
            }
            _ => bad("it isn't one a backup carries"),
        }
    }

    /// The archive's identities and syncs, in one write, with no maker: each waits (disabled,
    /// paused) until an admin resumes it, who becomes its maker then. Each identity is checked
    /// first as making it is ([`Secrets::admit_archived_identity`]); one that fails, or needs what
    /// isn't here (an environment, a gateway), is a warning.
    fn restore_workloads(&self, project: &str, identities: &[Row], syncs: &[Row], dek: &crate::crypto::Dek, caller: &Caller, warnings: &mut Vec<String>) -> Result<(usize, usize)> {
        if identities.is_empty() && syncs.is_empty() {
            return Ok((0, 0));
        }
        let complete = |r: &Row, columns: &[&str]| columns.iter().all(|c| r.columns.get(*c).is_some_and(|v| v.is_string() || v.is_number()));
        // Outside the write: an issuer's URL is checked through the guarded client.
        let mut admitted: Vec<(&Row, Vec<(&str, Cell)>)> = vec![];
        for r in identities {
            match self.admit_archived_identity(project, r, Some(caller)) {
                Ok(checked) => admitted.push((r, checked)),
                Err(e) => warnings.push(format!("identity {}: {e}; not restored", r.text("name"))),
            }
        }
        let now = self.db.now_ms()?;
        let (made, skipped) = self.db.write(|c| {
            let (mut made, mut skipped) = ((0, 0), vec![]);
            let mut count: i64 = c.query_row("SELECT COUNT(*) FROM identities WHERE project_id = ?", [project], |r| r.get(0))?;
            for (r, checked) in &admitted {
                let name = r.text("name");
                let envs: Vec<String> = serde_json::from_str(r.text("environments")).unwrap_or_default();
                let why = if let Some(e) = envs.iter().find(|e| envs::by_slug(c, project, e).is_err()) {
                    Some(format!("its environment {e} wasn't restored"))
                } else if count >= crate::identities::IDENTITIES_MAX {
                    Some(format!("a project has at most {} identities", crate::identities::IDENTITIES_MAX))
                } else {
                    None
                };
                if let Some(why) = why {
                    skipped.push(format!("identity {name}: {why}; not restored"));
                    continue;
                }
                let id = names::new_id(IDENTITIES.prefix, IDENTITIES.len);
                let mut extra = vec![("disabled_at", Cell::Int(now)), ("disabled_why", Cell::Text(RESTORED.into()))];
                extra.extend(checked.iter().cloned());
                backup_rows::import(c, &IDENTITIES, project, &id, "", now, r, dek, &extra)?;
                count += 1;
                made.0 += 1;
            }
            let mut count: i64 = c.query_row("SELECT COUNT(*) FROM syncs WHERE project_id = ?", [project], |r| r.get(0))?;
            let floors = crate::guardrails::read(c)?;
            // A sync roll secrets' policy wouldn't let the project make isn't restored either (a
            // product's own kinds aren't covered, as on create).
            let off_policy = |r: &Row| {
                let kind = r.text("kind");
                let target = self.syncs.get(kind).ok();
                if target.is_some_and(|t| t.internal()) {
                    return None;
                }
                let stored = target.and_then(|t| t.stored_cloud_key(&serde_json::from_str(r.text("target")).unwrap_or_default()));
                floors.sync(kind, stored).err().map(|e| e.to_string())
            };
            for r in syncs {
                let label = format!("{} sync on {}:{}", r.text("kind"), r.text("environment"), r.text("path"));
                let env = envs::by_slug(c, project, r.text("environment"));
                let why = if !complete(r, &["path", "kind", "target", "prefix", "state"]) {
                    Some("the backup doesn't hold all of it".to_string())
                } else if let Some(why) = off_policy(r) {
                    Some(why)
                } else if env.is_err() {
                    Some("its environment wasn't restored".into())
                } else if count >= crate::syncs::PER_PROJECT {
                    Some(format!("a project has at most {} syncs", crate::syncs::PER_PROJECT))
                } else {
                    None
                };
                let (Ok(env), None) = (env, why.clone()) else {
                    skipped.push(format!("{label}: {}; not restored", why.unwrap_or_default()));
                    continue;
                };
                let id = names::new_id(SYNCS.prefix, SYNCS.len);
                let extra = [
                    ("env_id", Cell::Text(env.id)),
                    ("status", Cell::Text("pending".into())),
                    ("running_until", Cell::Int(0)),
                    ("retry_at", Cell::Int(0)),
                    ("paused_at", Cell::Int(now)),
                    ("paused_why", Cell::Text(RESTORED.into())),
                ];
                backup_rows::import(c, &SYNCS, project, &id, "", now, r, dek, &extra)?;
                count += 1;
                made.1 += 1;
            }
            Ok::<_, Error>((made, skipped))
        })?;
        warnings.extend(skipped);
        Ok(made)
    }

    /// An archived identity checked as making it is ([`Secrets::admit_identity`]: the shared-issuer
    /// rule, the issuer URL, the TTL range, an AWS one's ARN, a machine one's empty fields), its
    /// address list and scope as they're read (the scope only what `caller` may grant, when it's a
    /// restore's), and none that logs a gateway in. Answers the columns to write as checked.
    fn admit_archived_identity(&self, project: &str, r: &Row, caller: Option<&Caller>) -> Result<Vec<(&'static str, Cell)>> {
        let complete = ["name", "issuer", "audience", "subject", "claims", "actions", "ttl_minutes"].iter().all(|c| r.columns.get(*c).is_some_and(|v| v.is_string() || v.is_number()));
        if !complete {
            return Err(Error::BadRequest("the backup doesn't hold all of it".into()));
        }
        if r.columns.get("gateway").is_some_and(|g| !g.is_null()) {
            return Err(Error::BadRequest("it logs a gateway in, which a backup doesn't carry".into()));
        }
        let name = r.text("name");
        if name.trim().is_empty() || name.chars().count() > 64 {
            return Err(Error::BadRequest("its name isn't one roll takes".into()));
        }
        let read = |column: &str| Error::BadRequest(format!("its {column} isn't one roll reads"));
        let kind = crate::identities::IdentityKind::from_stored(r.columns.get("kind").and_then(Json::as_str)).ok_or_else(|| read("kind"))?;
        let claims: BTreeMap<String, String> = serde_json::from_str(r.text("claims")).map_err(|_| read("claims"))?;
        let ttl = r.columns.get("ttl_minutes").and_then(Json::as_i64).ok_or_else(|| read("ttl_minutes"))?;
        let jwks = Some(r.text("jwks").to_string()).filter(|j| !j.is_empty());
        let (issuer, audience, subject, jwks, _) = self.admit_identity(project, kind, r.text("issuer").into(), r.text("audience").into(), r.text("subject").into(), &claims, jwks, ttl)?;
        let list = |column: &str| -> Result<Option<Vec<String>>> {
            match r.columns.get(column).and_then(Json::as_str) {
                None => Ok(None),
                Some(s) => serde_json::from_str(s).map(Some).map_err(|_| read(column)),
            }
        };
        let allowed_ips = crate::caller::allowed(list("allowed_ips")?)?;
        let actions: Vec<Action> = serde_json::from_str(r.text("actions")).map_err(|_| read("actions"))?;
        let (environments, paths, actions) = match caller {
            Some(caller) => crate::caller::scope(project, list("environments")?, list("paths")?, actions, caller)?,
            None => (list("environments")?, list("paths")?, actions),
        };
        let text = |v: Option<String>| v.map_or(Cell::Null, Cell::Text);
        let json_of = |v: &Option<Vec<String>>| text(v.as_ref().map(|l| serde_json::to_string(l).expect("JSON")));
        Ok(vec![
            ("issuer", Cell::Text(issuer)),
            ("audience", Cell::Text(audience)),
            ("subject", Cell::Text(subject)),
            ("jwks", text(jwks)),
            ("allowed_ips", json_of(&allowed_ips)),
            ("environments", json_of(&environments)),
            ("paths", json_of(&paths)),
            ("actions", Cell::Text(serde_json::to_string(&actions).expect("JSON"))),
            ("kind", text(kind.stored().map(str::to_string))),
        ])
    }

    /// Compare an opened archive with the live project, writing nothing: what a restore would
    /// bring back, what changed since, what the backup doesn't hold, what a restore would refuse.
    /// By names and counts only: values, keys and certificates are compared here and never
    /// answered. For a caller who may back the project up, and audited (it reads every value to
    /// compare). A token gets counts only, its registrations compared without their keys (it can't
    /// take those out either: [`carries_keys`]), and [`VERIFIES_PER_MINUTE`] calls a minute, as
    /// `/scan` limits a scanner: so it can't confirm guesses at what it can't read.
    pub fn verify_backup(&self, project: &str, archive: Archive, caller: &Caller) -> Result<Verified> {
        need_whole(caller, project)?;
        if !(1..=V).contains(&archive.v) {
            return Err(Error::BadRequest(format!("an archive of version {} (this server reads 1 to {V})", archive.v)));
        }
        if caller.principal != "operator" {
            let (calls, _) = self.count_calls(&format!("backup.verify:{}", caller.principal), 1)?;
            if calls > VERIFIES_PER_MINUTE {
                self.audit_now(project, caller, "backup.verify", None, None, &[], &format!("refused a verify: over {VERIFIES_PER_MINUTE} a minute"));
                return Err(Error::TooMany(format!("at most {VERIFIES_PER_MINUTE} verifies a minute: try again in a minute")));
            }
        }
        let names = carries_keys(caller);
        // What a restore would refuse, as it would: never the identities' scope (that's the restorer's).
        let mut refused = vec![];
        if names {
            for r in &archive.registrations {
                if let Some(t) = backup_rows::table(&r.kind)
                    && let Err(e) = self.admit(t, r)
                {
                    refused.push(format!("{}: {e}", registration_label(r)));
                }
            }
            for r in &archive.identities {
                if let Err(e) = self.admit_archived_identity(project, r, None) {
                    refused.push(format!("identity {}: {e}", r.text("name")));
                }
            }
        }
        let live = self.archive(project)?;
        let live_keys: usize = self.db.read(|c| Ok::<_, Error>(c.query_row("SELECT COUNT(*) FROM secrets WHERE project_id = ?", [project], |r| r.get::<_, i64>(0))? as usize))?;
        let envs = |a: &Archive| -> BTreeMap<String, [u8; 32]> { a.environments.iter().map(|e| (e.slug.clone(), fingerprint(&(&e.base, &e.labels, e.protected, e.approvals, &e.approvers, &e.folders)))).collect() };
        let keys = |a: &Archive| -> BTreeMap<String, [u8; 32]> {
            a.environments
                .iter()
                .flat_map(|e| e.secrets.iter().map(move |s| (key_label(&e.slug, &s.path, &s.key), fingerprint(&(s.link.as_ref().map(Link::fingerprint), &s.value)))))
                .collect()
        };
        let registrations = |a: &Archive| -> BTreeMap<String, [u8; 32]> {
            let mut out: BTreeMap<String, [u8; 32]> = a.registrations.iter().map(|r| (registration_label(r), fingerprint(&(&r.columns, names.then_some(&r.sealed), &r.certificates)))).collect();
            out.extend(a.encryption_keys.iter().map(|k| (format!("encryption key {}", k.name), fingerprint(&k.versions))));
            out
        };
        let identities = |a: &Archive| -> BTreeMap<String, [u8; 32]> { a.identities.iter().map(|r| (format!("identity {}", r.text("name")), fingerprint(&r.columns))).collect() };
        let syncs = |a: &Archive| -> BTreeMap<String, [u8; 32]> {
            a.syncs
                .iter()
                .map(|r| {
                    let label = format!("{} sync on {}:{}", r.text("kind"), r.text("environment"), r.text("path"));
                    (label, fingerprint(&(r.columns.get("target"), r.columns.get("keys"), r.columns.get("prefix"))))
                })
                .collect()
        };
        let (a, l) = (archive.settings.clone().unwrap_or_default(), live.settings.clone().unwrap_or_default());
        let mut settings = vec![];
        if archive.settings.is_some() {
            if a.history_days != l.history_days {
                settings.push("history_days".to_string());
            }
            if a.validity_checks != l.validity_checks {
                settings.push("validity_checks".into());
            }
            if a.leak_rotate.as_deref().unwrap_or("off") != l.leak_rotate.as_deref().unwrap_or("off") {
                settings.push("leaks.rotate".into());
            }
        }
        let v = Verified {
            environments: diff(envs(&archive), envs(&live), names),
            keys: diff(keys(&archive), keys(&live), names),
            registrations: diff(registrations(&archive), registrations(&live), names),
            identities: diff(identities(&archive), identities(&live), names),
            syncs: diff(syncs(&archive), syncs(&live), names),
            refused,
            names,
            settings,
            left_out: archive.left_out,
            live_keys,
            project: archive.project,
            at: archive.at,
            v: archive.v,
        };
        let summary = format!(
            "checked a backup of {} against the project: {} keys missing, {} changed, {} registrations missing, {} changed (nothing written)",
            v.project, v.keys.counts.missing, v.keys.counts.changed, v.registrations.counts.missing, v.registrations.counts.changed
        );
        self.audit_now(project, caller, "backup.verify", None, None, &[], &summary);
        Ok(v)
    }

    /// The newest scheduled backup in the project's bucket, still sealed (`{sealed, context,
    /// key}`): for `roll secrets backup verify --latest-from-bucket`, which opens it where the key
    /// is. roll can't open it either.
    pub fn latest_backup(&self, project: &str, caller: &Caller) -> Result<Json> {
        let schedule = self.backup_schedule(project, caller)?.ok_or_else(|| Error::NotFound("the project isn't backed up on a schedule".into()))?;
        let key = schedule.last_key.ok_or_else(|| Error::NotFound("no scheduled backup has landed yet".into()))?;
        let deliver = self.deliver.as_ref().ok_or_else(|| Error::BadRequest("scheduled backups aren't on this server".into()))?;
        // As a run does: only from a bucket the project still has.
        if !self.members(project)?.contains(&("bucket".to_string(), schedule.bucket.clone())) {
            return Err(Error::BadRequest(self.not_linkable(project, "bucket", &schedule.bucket)));
        }
        let mut v = deliver.get_object(&schedule.bucket, &key, LATEST_MAX)?;
        if !v.is_object() || !v["sealed"].is_object() {
            return Err(Error::Link(format!("{key} in {} isn't a sealed backup", schedule.bucket)));
        }
        v["key"] = Json::String(key);
        Ok(v)
    }
}

/// A backup someone took (or, asked for, a verify they ran), as its audit row says: who, when,
/// and the registrations it carried with their keys (by name: `CA internal`).
pub(crate) struct Taken {
    /// Who took it: a person, `operator`, `token:…`; a scheduled one is its schedule's maker's.
    pub actor: String,
    pub at: i64,
    pub registrations: Vec<String>,
}

/// How far back health and offboarding look for backups someone took: the project's history
/// (`history_days`), or 90 days where it keeps everything.
pub(crate) fn taken_days(history_days: Option<i64>) -> i64 {
    history_days.unwrap_or(90)
}

/// The most backup rows read for reach, health and offboarding.
const TAKEN_MAX: i64 = 10_000;

impl Secrets {
    /// The project's backups since `since` (ms), newest first; with `verifies`, its verifies too
    /// (they read every value to compare). For reach, health and offboarding: what left, with whom.
    pub(crate) fn backups_taken(&self, project: &str, since: i64, verifies: bool) -> Result<Vec<Taken>> {
        let actions = if verifies { "'project.backup', 'backup.verify'" } else { "'project.backup'" };
        let (rows, schedule): (Vec<(String, i64, String)>, Option<String>) = self.db.read(|c| {
            let rows = c.query(&format!("SELECT actor, at, keys FROM audit WHERE project_id = ? AND at >= ? AND action IN ({actions}) ORDER BY id DESC LIMIT ?"), params![project, since, TAKEN_MAX], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            let schedule = c.query_row("SELECT created_by FROM backup_schedules WHERE project_id = ?", [project], |r| r.get(0)).optional()?;
            Ok::<_, Error>((rows, schedule))
        })?;
        let scheduled = format!("backup:{project}");
        Ok(rows
            .into_iter()
            .map(|(actor, at, keys)| {
                let actor = if actor == scheduled { schedule.clone().unwrap_or(actor) } else { actor };
                Taken { actor, at, registrations: crate::from_json(&keys) }
            })
            .collect())
    }
}

/// What to do about a registration a backup carried out, now its taker has left: a CA is
/// re-issued (a new CA, its certificates moved to it), a credential is rotated at its service.
pub(crate) fn advice(registration: &str) -> String {
    if registration.starts_with("CA ") || registration.starts_with("SSH CA ") {
        format!("{registration}: re-issue it (its key left in the backup)")
    } else if registration.starts_with("ACME account") {
        format!("{registration}: deactivate it at the CA")
    } else {
        format!("{registration}: rotate its credential at its service")
    }
}

/// The most of a scheduled backup `latest_backup` reads.
const LATEST_MAX: usize = 64 << 20;

/// A SHA-256 of what's compared: never answered, and the serialization is wiped.
fn fingerprint(v: &impl Serialize) -> [u8; 32] {
    use sha2::Digest;
    let bytes = zeroize::Zeroizing::new(serde_json::to_vec(v).expect("JSON"));
    sha2::Sha256::digest(&*bytes).into()
}

/// `archive` against `live`, by name; with `names` false, counts only.
fn diff(archive: BTreeMap<String, [u8; 32]>, live: BTreeMap<String, [u8; 32]>, names: bool) -> Diff {
    let missing: Vec<String> = archive.keys().filter(|k| !live.contains_key(*k)).cloned().collect();
    let extra: Vec<String> = live.keys().filter(|k| !archive.contains_key(*k)).cloned().collect();
    let changed: Vec<String> = archive.iter().filter(|(k, v)| live.get(*k).is_some_and(|l| l != *v)).map(|(k, _)| k.clone()).collect();
    let counts = Counts { missing: missing.len(), extra: extra.len(), changed: changed.len() };
    let named = |l: Vec<String>| if names { l } else { vec![] };
    Diff { archive: archive.len(), live: live.len(), missing: named(missing), extra: named(extra), changed: named(changed), counts }
}

fn key_label(env: &str, path: &str, key: &str) -> String {
    format!("{env}:{}{key}", if path == "/" { "/".to_string() } else { format!("{path}/") })
}

fn registration_label(r: &Row) -> String {
    let what = backup_rows::table(&r.kind).map_or(r.kind.as_str(), |t| t.what);
    format!("{what} {}", r.text("name"))
}

/// The associated data of a backup: bound to the project it was taken from.
pub fn context(project: &str) -> String {
    format!("roll-secrets-backup:{project}")
}

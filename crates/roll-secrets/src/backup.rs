//! Encrypted backup and restore (docs/secrets.md, "Backup and restore"): a project's environments,
//! folders, own keys, imports, policies and required keys as one archive, sealed to a key only the
//! caller holds (`roll-seal-v1`), and loaded back into an empty project, on this server or another.
//!
//! Linked keys carry their link and no value: a restore issues its own credentials. Personal
//! values, history, leases and tokens aren't in it.

use crate::access::{Action, Caller, Scope};
use crate::envs::{self, Kind};
use crate::{Error, Import, Link, Op, Required, Result, Secrets, Value, audit, from_json};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The archive's version.
const V: u8 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct Archive {
    pub v: u8,
    /// The project it was taken from (a restore may load it into another).
    pub project: String,
    pub at: i64,
    pub environments: Vec<ArchivedEnvironment>,
    #[serde(default)]
    pub required: Vec<Required>,
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
    /// What couldn't be restored and why (an import of another project's folder, a link to a
    /// resource this project doesn't have): names, never values.
    pub warnings: Vec<String>,
}

impl Secrets {
    /// The project's archive sealed to `recipient` (an X25519 public key). Every value of the
    /// project leaves in it, so it's an admin's, and audited.
    pub fn backup(&self, project: &str, recipient: &str, caller: &Caller) -> Result<crate::crypto::Sealed> {
        caller.need(Action::Admin, &Scope::project(project))?;
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
                    let value = match (&link, ct) {
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
            Ok::<_, Error>(Archive { v: V, project: project.to_string(), at: now, environments, required: crate::required::of(c, project)? })
        })?;
        let plain = zeroize::Zeroizing::new(serde_json::to_vec(&archive).expect("JSON"));
        let sealed = crate::crypto::seal_to(recipient, &context(project), &plain).map_err(Error::BadRequest)?;
        let n: usize = archive.environments.iter().map(|e| e.secrets.len()).sum();
        self.audit_now(project, caller, "project.backup", None, None, &[], &format!("backed up {} environments and {} keys, sealed to a key of the caller's", archive.environments.len(), n));
        Ok(sealed)
    }

    /// Load `archive` into `project`, which has no secrets yet. Values are written before any
    /// protection is set (a restore isn't a change request); what can't be restored is a warning.
    pub fn restore(&self, project: &str, archive: Archive, caller: &Caller) -> Result<Restored> {
        caller.need(Action::Admin, &Scope::project(project))?;
        if archive.v != V {
            return Err(Error::BadRequest(format!("an archive of version {} (this server reads {V})", archive.v)));
        }
        self.project(project)?;
        let used: i64 = self.db.read(|c| Ok::<_, Error>(c.query_row("SELECT COUNT(*) FROM secrets WHERE project_id = ?", [project], |r| r.get(0))?))?;
        if used > 0 {
            return Err(Error::Conflict("conflict", "a restore goes into a project with no secrets yet".into()));
        }
        let mut warnings = vec![];
        let (mut environments, mut secrets) = (0, 0);
        // Standard environments first (bases), then ephemeral ones; each as it was.
        let mut order: Vec<&ArchivedEnvironment> = archive.environments.iter().filter(|e| e.kind == Kind::Standard).collect();
        order.extend(archive.environments.iter().filter(|e| e.kind == Kind::Ephemeral));
        for e in &order {
            let ensure = crate::Ensure { name: Some(e.name.clone()), kind: Some(e.kind), base: e.base.clone(), labels: Some(e.labels.clone()), expires_at: e.expires_at, trusted: e.trusted, ..Default::default() };
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
                    link: s.link.clone().map(Some),
                    path: None,
                    expires_at: s.expires_at.map(Some),
                });
            }
            for (path, ops) in by_path {
                let write = |ops: Vec<Op>| self.batch(project, &e.slug, crate::Batch { path: Some(path.to_string()), message: Some("restored from a backup".into()), ops, if_seq: None }, caller);
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
        for e in order.iter().filter(|e| e.protected) {
            let p = envs::Policy { protected: true, approvals: Some(e.approvals), approvers: Some(e.approvers.clone()), notify_url: None };
            if let Err(err) = self.set_policy(project, &e.slug, p, caller) {
                warnings.push(format!("{} policy: {err}", e.slug));
            }
        }
        let now = self.db.now_ms()?;
        self.db.write(|c| audit::record(c, project, caller, "project.restore", None, None, &[], &format!("restored {environments} environments and {secrets} keys from {}'s backup", archive.project), now))?;
        Ok(Restored { environments, secrets, warnings })
    }
}

/// The associated data of a backup: bound to the project it was taken from.
pub fn context(project: &str) -> String {
    format!("roll-secrets-backup:{project}")
}

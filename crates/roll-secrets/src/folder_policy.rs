//! Change policies per folder (docs/secrets.md, "Protected environments and approvals"), as
//! Infisical's approval policies on secret paths: a folder (`/payments`) of an environment that
//! isn't protected as a whole still takes changes only through approved requests, with its own
//! approvals and approvers. A policy covers its folder and every folder under it.
//!
//! A change's **gates** are the environment's policy (when it's protected) and every folder policy
//! covering a key it touches; a request is approved when each gate has its approvals from its own
//! approvers. Only who may change what is folder by folder: reading (untrusted readers, exposure)
//! stays the environment's.

use crate::access::{Action, Caller, Scope};
use crate::envs::{self, EnvRow, Kind};
use crate::{Error, Result, Secrets, audit, from_json, json, names};
use code_sql::{Conn, OptionalExtension, params};
use serde::{Deserialize, Serialize};

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS folder_policies (
    env_id TEXT NOT NULL,
    path TEXT NOT NULL,
    approvals INTEGER NOT NULL,
    approvers TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    PRIMARY KEY (env_id, path)
) WITHOUT ROWID;
";

/// The most folder policies one environment has.
const MAX: i64 = 200;

#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct FolderPolicy {
    pub path: String,
    pub approvals: i64,
    /// Who may approve; empty: anyone who may write here, other than the author.
    pub approvers: Vec<String>,
    pub created_at: i64,
    pub created_by: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetFolderPolicy {
    #[serde(default)]
    pub approvals: Option<i64>,
    #[serde(default)]
    pub approvers: Option<Vec<String>>,
}

/// One gate a change passes: the environment's policy (`path: None`) or a folder's.
#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct Gate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub approvals: i64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub approvers: Vec<String>,
}

fn covers(policy: &str, path: &str) -> bool {
    policy == "/" || path == policy || path.starts_with(&format!("{policy}/"))
}

pub(crate) fn all(c: &Conn<'_>, env_id: &str) -> Result<Vec<FolderPolicy>> {
    Ok(c.query("SELECT path, approvals, approvers, created_at, created_by FROM folder_policies WHERE env_id = ? ORDER BY path", [env_id], |r| {
        Ok(FolderPolicy { path: r.get(0)?, approvals: r.get(1)?, approvers: from_json(&r.get::<_, String>(2)?), created_at: r.get(3)?, created_by: r.get(4)? })
    })?)
}

/// The folder policies covering any of `paths`.
pub(crate) fn covering(c: &Conn<'_>, env_id: &str, paths: &[&str]) -> Result<Vec<FolderPolicy>> {
    Ok(all(c, env_id)?.into_iter().filter(|p| paths.iter().any(|x| covers(&p.path, x))).collect())
}

/// A change's gates in `env`, for keys in `paths`. None at all (a request on an environment with
/// no policy): the environment's, as it always was.
pub(crate) fn gates(c: &Conn<'_>, env: &EnvRow, paths: &[&str]) -> Result<Vec<Gate>> {
    let mut out = vec![];
    if env.protected {
        out.push(Gate { path: None, approvals: env.approvals.max(1), approvers: env.approvers.clone() });
    }
    out.extend(covering(c, &env.id, paths)?.into_iter().map(|p| Gate { path: Some(p.path), approvals: p.approvals.max(1), approvers: p.approvers }));
    if out.is_empty() {
        out.push(Gate { path: None, approvals: env.approvals.max(1), approvers: env.approvers.clone() });
    }
    Ok(out)
}

/// Approvals from `reviews` that count for `gate`: distinct approvers other than the author, among
/// the gate's approvers when it names them, whose review counts for this gate (they were admin of
/// its scope).
pub(crate) fn counted(gate: &Gate, reviews: &[crate::requests::Review], author: &str) -> i64 {
    let mut by: Vec<&str> = reviews
        .iter()
        .filter(|r| r.verdict == "approve" && r.by != author && (gate.approvers.is_empty() || gate.approvers.contains(&r.by)) && r.gates.as_ref().is_none_or(|g| g.contains(&gate.path)))
        .map(|r| r.by.as_str())
        .collect();
    by.sort();
    by.dedup();
    by.len() as i64
}

/// Every gate has its approvals.
pub(crate) fn satisfied(gates: &[Gate], reviews: &[crate::requests::Review], author: &str) -> bool {
    gates.iter().all(|g| counted(g, reviews, author) >= g.approvals)
}

/// `who` may review: an approver of some gate (or a gate takes anyone).
pub(crate) fn may_review(gates: &[Gate], who: &str) -> bool {
    gates.iter().any(|g| g.approvers.is_empty() || g.approvers.iter().any(|a| a == who))
}

impl Secrets {
    pub fn folder_policies(&self, project: &str, slug: &str, caller: &Caller) -> Result<Vec<FolderPolicy>> {
        caller.need(Action::Read, &Scope::env(project, slug))?;
        self.project(project)?;
        self.db.read(|c| {
            let env = envs::by_slug(c, project, slug)?;
            all(c, &env.id)
        })
    }

    /// Set the policy on `path` (admin): its approvals (1–10, 1 by default) and approvers.
    pub fn set_folder_policy(&self, project: &str, slug: &str, path: &str, p: SetFolderPolicy, caller: &Caller) -> Result<FolderPolicy> {
        let path = names::path(path)?;
        caller.need(Action::Admin, &Scope::env(project, slug))?;
        let approvals = p.approvals.unwrap_or(1);
        if !(1..=10).contains(&approvals) {
            return Err(Error::BadRequest("approvals is 1–10".into()));
        }
        let approvers = p.approvers.unwrap_or_default();
        if approvers.len() > 50 || approvers.iter().any(|a| a.is_empty() || a.len() > 200) {
            return Err(Error::BadRequest("at most 50 approvers, each a principal".into()));
        }
        self.project(project)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let env = envs::by_slug(c, project, slug)?;
            if env.kind == Kind::Ephemeral {
                return Err(Error::BadRequest("an ephemeral environment's folders can't have a change policy".into()));
            }
            let have: i64 = c.query_row("SELECT COUNT(*) FROM folder_policies WHERE env_id = ?", [&env.id], |r| r.get(0))?;
            let exists = c.query_row("SELECT 1 FROM folder_policies WHERE env_id = ? AND path = ?", [&env.id, &path], |r| r.get::<_, i64>(0)).optional()?.is_some();
            if !exists && have >= MAX {
                return Err(Error::BadRequest(format!("an environment has at most {MAX} folder policies")));
            }
            c.execute("DELETE FROM folder_policies WHERE env_id = ? AND path = ?", [&env.id, &path])?;
            c.execute(
                "INSERT INTO folder_policies (env_id, path, approvals, approvers, created_at, created_by) VALUES (?, ?, ?, ?, ?, ?)",
                params![env.id, path, approvals, json(&approvers), now, caller.principal],
            )?;
            audit::record(c, project, caller, "folder.policy", Some(slug), Some(&path), &[], &format!("changes to {path} need {approvals} approval{}", if approvals == 1 { "" } else { "s" }), now)?;
            Ok(FolderPolicy { path: path.clone(), approvals, approvers: approvers.clone(), created_at: now, created_by: caller.principal.clone() })
        })
    }

    pub fn delete_folder_policy(&self, project: &str, slug: &str, path: &str, caller: &Caller) -> Result<()> {
        let path = names::path(path)?;
        caller.need(Action::Admin, &Scope::env(project, slug))?;
        self.project(project)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let env = envs::by_slug(c, project, slug)?;
            if c.execute("DELETE FROM folder_policies WHERE env_id = ? AND path = ?", [&env.id, &path])? == 0 {
                return Err(Error::NotFound(format!("{path} in {slug} has no change policy")));
            }
            audit::record(c, project, caller, "folder.policy", Some(slug), Some(&path), &[], &format!("changes to {path} no longer need approval"), now)
        })
    }
}

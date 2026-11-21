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

/// The project's members' roles now (an approver may be named by role: `role:admin`,
/// `role:owner`, docs/members.md), and, when a request is applied, which reviewers' approvals still
/// count: a person who was removed or demoted since no longer does. Looked up before the write that
/// uses it (people's memberships are the workspace's).
#[derive(Debug, Default)]
pub(crate) struct Standing {
    pub(crate) roles: std::collections::HashMap<String, roll_auth::roles::Role>,
    /// `(reviewer, gate path)` pairs that count now; `None`: every review as recorded.
    pub(crate) counts: Option<std::collections::HashSet<(String, Option<String>)>>,
}

/// Whether `who` is one of `gate`'s approvers: named, by role in the project, or anyone at all.
fn approver(gate: &Gate, who: &str, st: &Standing) -> bool {
    gate.approvers.is_empty()
        || gate.approvers.iter().any(|a| match a.strip_prefix("role:").and_then(roll_auth::roles::Role::parse) {
            Some(role) => st.roles.get(who) == Some(&role),
            None => a == who,
        })
}

/// Approvals from `reviews` that count for `gate`: distinct approvers other than the author, among
/// the gate's approvers when it names them, whose review counts for this gate (they were admin of
/// its scope, and still are when `st` says who counts now).
pub(crate) fn counted(gate: &Gate, reviews: &[crate::requests::Review], author: &str, st: &Standing) -> i64 {
    let mut by: Vec<&str> = reviews
        .iter()
        .filter(|r| r.verdict == "approve" && r.by != author && approver(gate, &r.by, st) && r.gates.as_ref().is_none_or(|g| g.contains(&gate.path)))
        .filter(|r| st.counts.as_ref().is_none_or(|c| c.contains(&(r.by.clone(), gate.path.clone()))))
        .map(|r| r.by.as_str())
        .collect();
    by.sort();
    by.dedup();
    by.len() as i64
}

/// Every gate has its approvals.
pub(crate) fn satisfied(gates: &[Gate], reviews: &[crate::requests::Review], author: &str, st: &Standing) -> bool {
    gates.iter().all(|g| counted(g, reviews, author, st) >= g.approvals)
}

/// `who` may review: an approver of some gate (or a gate takes anyone).
pub(crate) fn may_review(gates: &[Gate], who: &str, st: &Standing) -> bool {
    gates.iter().any(|g| approver(g, who, st))
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
            audit::record(c, self.keys()?, project, caller, "folder.policy", Some(slug), Some(&path), &[], &format!("changes to {path} need {approvals} approval{}", if approvals == 1 { "" } else { "s" }), now)?;
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
            audit::record(c, self.keys()?, project, caller, "folder.policy", Some(slug), Some(&path), &[], &format!("changes to {path} no longer need approval"), now)
        })
    }
}

//! Credentials roll code found in a change that roll secrets doesn't hold yet, stored from the
//! change itself (docs/secrets.md, "Credentials roll doesn't hold": the dashboard's "Store in
//! roll"). roll code reads the value from the change's blob and hands it here in process; this
//! module picks the project the repository is trusted for and writes it as the caller, never
//! returning it.

use crate::access::{Action, Caller, Scope};
use crate::write::{Batch, Op};
use crate::{Error, Result, Secrets, names};

/// Where a repository's found credential would go: the project roll secrets trusts the repository
/// for, the environments the caller may read in it, and the one to suggest.
#[derive(Clone, Debug, PartialEq)]
pub struct FoundTarget {
    pub project: String,
    pub project_name: String,
    /// Slugs, in the project's order.
    pub environments: Vec<String>,
    /// `prefer` when the project has it, else `development`, else none (the caller picks).
    pub environment: Option<String>,
}

/// What storing it did: one change, or a change request where the folder needs approval.
#[derive(Clone, Debug, PartialEq)]
pub struct FoundStored {
    pub project: String,
    pub environment: String,
    pub path: String,
    /// The change's number in the environment.
    pub change: Option<i64>,
    /// The change request opened instead (`req_…`).
    pub request: Option<String>,
}

/// The environment suggested when nothing names one.
const DEFAULT_ENV: &str = "development";

impl Secrets {
    /// The project `repo` (`owner/name`) is trusted for that the caller can see: the first with
    /// the repository on its canvas that counts for it ([`Secrets::counts_for_repo`], never canvas
    /// membership alone). 409 `no_project` when none trusts it.
    fn found_project(&self, repo: &str, caller: &Caller) -> Result<(String, String)> {
        let projects = self.workspace.projects_of(roll_workspace::Kind::Repo.as_str(), repo).map_err(|e| Error::Internal(e.to_string()))?;
        let trusted: Vec<String> = projects.into_iter().filter(|p| self.counts_for_repo(p, repo)).collect();
        if trusted.is_empty() {
            return Err(Error::Conflict("no_project", format!("{repo} isn't on a project roll secrets trusts it for: put it on the canvas of a project of its owner's")));
        }
        let id = trusted.iter().find(|p| caller.grants.sees(p)).ok_or_else(|| Error::Forbidden(format!("you may not use roll secrets of the project {repo} is on")))?;
        let name = self.workspace.get(roll_workspace::Scope::All, id).map(|p| p.name).unwrap_or_else(|_| id.clone());
        Ok((id.clone(), name))
    }

    /// Where a credential found in `repo` would be stored, suggesting `prefer` (the environment
    /// the repository's `roll.toml` names) when the project has it.
    pub fn found_target(&self, repo: &str, prefer: Option<&str>, caller: &Caller) -> Result<FoundTarget> {
        let (project, project_name) = self.found_project(repo, caller)?;
        let environments: Vec<String> = self.environments(&project, caller)?.into_iter().map(|e| e.slug).collect();
        let environment = [prefer, Some(DEFAULT_ENV)].into_iter().flatten().find(|e| environments.iter().any(|x| x == e)).map(str::to_string);
        Ok(FoundTarget { project, project_name, environments, environment })
    }

    /// Store `value` (a credential roll code read from a change of `repo`) as a new `key` of
    /// `environment` (default: [`Self::found_target`]'s) at `path`, as `caller`: `write` there,
    /// one change, or a change request where the environment or folder needs approval. A key the
    /// folder already has is refused (409 `exists`), never overwritten.
    #[allow(clippy::too_many_arguments)]
    pub fn store_found(&self, repo: &str, environment: Option<&str>, prefer: Option<&str>, path: &str, key: &str, value: String, message: String, caller: &Caller) -> Result<FoundStored> {
        let target = self.found_target(repo, prefer, caller)?;
        let env = match environment {
            Some(e) => e.to_string(),
            None => target.environment.clone().ok_or_else(|| Error::BadRequest(format!("name an environment: {} has no {DEFAULT_ENV}", target.project_name)))?,
        };
        let path = names::path(path)?;
        let project = target.project;
        caller.need(Action::Write, &Scope::at(&project, &env, &path))?;
        if self.secret(&project, &env, &path, key, None, caller).is_ok() {
            return Err(Error::Conflict("exists", format!("{key} is already set in {env}:{path}: name another")));
        }
        // `if_version: 0`: only while the key doesn't exist, checked in the write itself.
        let batch = || Batch {
            path: Some(path.clone()),
            message: Some(message.clone()),
            ops: vec![Op::Set { key: key.to_string(), value: Some(value.clone()), comment: None, tags: None, kind: None, link: None, path: None, expires_at: None, if_version: Some(0) }],
            if_seq: None,
        };
        match self.batch(&project, &env, batch(), caller) {
            Ok(a) => Ok(FoundStored { project, environment: env, path, change: a.change.map(|c| c.seq), request: None }),
            Err(Error::Conflict("approval_required", _)) => {
                let r = self.open_request(&project, &env, batch(), caller)?;
                Ok(FoundStored { project, environment: env, path, change: None, request: Some(r.id) })
            }
            Err(e) => Err(e),
        }
    }
}

//! Who is calling and what they may do (docs/secrets.md, "Access").
//!
//! The roll-wide permission model (docs/permissions.md) classifies each request in rolld's gate.
//! Inside the product, the checks that the gate can't make from the request alone (a second
//! environment, a path inside a batch, each finding of a filtered answer) go through [`Grants`]:
//! the operator (everything), a roll-access grant (`rlt_…`/`rlo_…`, [`GrantGrants`]) or one of the
//! project's own scoped tokens (`rlst_…`, [`TokenScope`]).

use crate::names;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// Names, metadata, versions, history; never a value.
    Read,
    /// Values.
    Reveal,
    Write,
    Delete,
    /// Environment settings, protection, reviewing requests.
    Admin,
}

impl Action {
    pub const ALL: [Action; 5] = [Action::Read, Action::Reveal, Action::Write, Action::Delete, Action::Admin];

    pub fn as_str(self) -> &'static str {
        match self {
            Action::Read => "read",
            Action::Reveal => "reveal",
            Action::Write => "write",
            Action::Delete => "delete",
            Action::Admin => "admin",
        }
    }
}

/// What an action is on: a project, and optionally one environment and folder in it.
#[derive(Clone, Copy, Debug)]
pub struct Scope<'a> {
    pub project: &'a str,
    pub env: Option<&'a str>,
    pub path: Option<&'a str>,
}

impl<'a> Scope<'a> {
    pub fn project(project: &'a str) -> Self {
        Scope { project, env: None, path: None }
    }

    pub fn env(project: &'a str, env: &'a str) -> Self {
        Scope { project, env: Some(env), path: None }
    }

    pub fn at(project: &'a str, env: &'a str, path: &'a str) -> Self {
        Scope { project, env: Some(env), path: Some(path) }
    }
}

/// What a caller may do.
pub trait Grants: Send + Sync + std::fmt::Debug {
    fn allows(&self, action: Action, at: &Scope<'_>) -> bool;
    /// The one project the caller is limited to, if any (`whoami`, and the CLI's default).
    fn project(&self) -> Option<&str> {
        None
    }
    /// A consumer running code nobody vetted (`Secrets::untrusted`): not even a folder another
    /// project shares reaches it from a protected environment.
    fn untrusted(&self) -> bool {
        false
    }
    /// Whether the caller may read repository `repo`'s (`owner/name`, on `project`'s canvas) code:
    /// health's `code` check says something about a repository's contents. roll-access answers from
    /// the caller's Grant (`code`, the repo in that project, `read`); here only the operator may.
    fn reads_code(&self, project: &str, repo: &str) -> bool {
        let _ = (project, repo);
        false
    }
    /// Whether the caller may know `project` exists: a person's own projects only, so anyone
    /// else's answers 404, as the workspace does.
    fn sees(&self, project: &str) -> bool {
        let _ = project;
        true
    }
    /// The operator itself: what's about the whole server (the deployment key) is only theirs.
    fn everything(&self) -> bool {
        false
    }
}

/// A signed-in person who isn't an admin (docs/accounts.md): everything on the projects they own,
/// nothing on anyone else's. Looked up per request (`Secrets::caller`), never kept.
#[derive(Debug)]
pub struct OwnProjects {
    pub projects: std::collections::BTreeSet<String>,
    /// Their roll code owner's name: the repositories under it are theirs (`owner/…`).
    pub code_owner: Option<String>,
}

impl OwnProjects {
    /// Whether `repo` (`owner/name`) is under the person's own roll code owner.
    pub fn owns_repo(code_owner: Option<&str>, repo: &str) -> bool {
        code_owner.is_some_and(|o| repo.split_once('/').is_some_and(|(owner, _)| owner == o))
    }
}

impl Grants for OwnProjects {
    fn allows(&self, _: Action, at: &Scope<'_>) -> bool {
        self.projects.contains(at.project)
    }
    fn project(&self) -> Option<&str> {
        let mut it = self.projects.iter();
        match (it.next(), it.next()) {
            (Some(p), None) => Some(p),
            _ => None,
        }
    }
    fn reads_code(&self, project: &str, repo: &str) -> bool {
        self.projects.contains(project) && OwnProjects::owns_repo(self.code_owner.as_deref(), repo)
    }
    fn sees(&self, project: &str) -> bool {
        self.projects.contains(project)
    }
}

/// People's accounts as they are now (docs/accounts.md; rolld asks roll-accounts). What a person
/// made that acts later (a sync, a token, an identity) acts only while they can still sign in.
pub trait People: Send + Sync {
    /// `None` when the account is gone or disabled; an error when it couldn't be looked up.
    fn person(&self, user: &str) -> Result<Option<Person>, String>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Person {
    pub admin: bool,
    /// Their roll code owner's name, if they have one.
    pub code_owner: Option<String>,
}

/// A person's user id (`usr_…`), as a principal.
pub fn is_person(principal: &str) -> bool {
    principal.starts_with("usr_")
}

/// A grant a person made, acting as them: what its rules allow, and only on the person's own
/// projects (a person's token never reaches further than their session does).
#[derive(Debug)]
pub struct Within(pub GrantGrants, pub OwnProjects);

impl Grants for Within {
    fn allows(&self, action: Action, at: &Scope<'_>) -> bool {
        self.1.allows(action, at) && self.0.allows(action, at)
    }
    fn project(&self) -> Option<&str> {
        self.0.project().or_else(|| self.1.project())
    }
    fn reads_code(&self, project: &str, repo: &str) -> bool {
        self.1.reads_code(project, repo) && self.0.reads_code(project, repo)
    }
    fn sees(&self, project: &str) -> bool {
        self.1.sees(project)
    }
}

/// The operator: everything.
#[derive(Debug)]
pub struct Everything;

impl Grants for Everything {
    fn allows(&self, _: Action, _: &Scope<'_>) -> bool {
        true
    }
    fn everything(&self) -> bool {
        true
    }
    fn reads_code(&self, _: &str, _: &str) -> bool {
        true
    }
}

/// Everything except the environments it names: what checks and sandboxes get, so a protected
/// environment never reaches them, through a reference, an import or a base either.
#[derive(Debug)]
pub struct Except(pub Vec<String>);

impl Grants for Except {
    fn allows(&self, _: Action, at: &Scope<'_>) -> bool {
        at.env.is_none_or(|e| !self.0.iter().any(|x| x == e))
    }
}

/// `0`'s grants, never in `1`'s environments.
#[derive(Debug)]
struct Narrowed(std::sync::Arc<dyn Grants>, Except);

impl Grants for Narrowed {
    fn allows(&self, action: Action, at: &Scope<'_>) -> bool {
        self.1.allows(action, at) && self.0.allows(action, at)
    }
    fn project(&self) -> Option<&str> {
        self.0.project()
    }
    fn untrusted(&self) -> bool {
        true
    }
    fn reads_code(&self, project: &str, repo: &str) -> bool {
        self.0.reads_code(project, repo)
    }
    fn sees(&self, project: &str) -> bool {
        self.0.sees(project)
    }
}

/// A scoped token's grants: one project; environments and folders (prefixes) or all; actions.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TokenScope {
    pub project: String,
    pub environments: Option<Vec<String>>,
    pub paths: Option<Vec<String>>,
    pub actions: Vec<Action>,
}

impl Grants for TokenScope {
    fn allows(&self, action: Action, at: &Scope<'_>) -> bool {
        if at.project != self.project || !self.actions.contains(&action) {
            return false;
        }
        // A token limited to some environments or folders only acts on project-wide things it
        // can read (the list of environments); everything else names its environment.
        if let Some(envs) = &self.environments {
            match at.env {
                Some(e) if envs.iter().any(|x| x == e) => {}
                None if action == Action::Read => {}
                _ => return false,
            }
        }
        if let Some(paths) = &self.paths {
            match at.path {
                Some(p) if paths.iter().any(|x| names::under(p, x)) => {}
                None if action == Action::Read => {}
                _ => return false,
            }
        }
        true
    }

    fn project(&self) -> Option<&str> {
        Some(&self.project)
    }
}

/// A roll-access grant (`rlt_…` tokens and OAuth connections, docs/permissions.md): rolld's gate
/// classified the request already; this answers the second resources the product checks itself.
#[derive(Debug)]
pub struct GrantGrants(pub roll_auth::Grant);

impl Grants for GrantGrants {
    fn allows(&self, action: Action, at: &Scope<'_>) -> bool {
        use roll_auth::scope;
        let action = match action {
            Action::Read => scope::Action::Read,
            Action::Reveal => scope::Action::Reveal,
            Action::Write => scope::Action::Write,
            Action::Delete => scope::Action::Delete,
            Action::Admin => scope::Action::Admin,
        };
        let r = scope::Resource { product: "secrets".into(), project: Some(at.project.into()), environment: at.env.map(Into::into), path: at.path.map(Into::into), ..Default::default() };
        // `ask` pauses a whole request at the gate; inside the product it's a no.
        self.0.allows(&r, action) == scope::Outcome::Allow
    }

    /// Code read on the repository, as it sits on `project`'s canvas.
    fn reads_code(&self, project: &str, repo: &str) -> bool {
        use roll_auth::scope;
        let r = scope::Resource { product: "code".into(), id: Some(repo.into()), project: Some(project.into()), ..Default::default() };
        self.0.allows(&r, scope::Action::Read) == scope::Outcome::Allow
    }

    /// The one project its secrets rules name, if they name exactly one.
    fn project(&self) -> Option<&str> {
        use roll_auth::scope::{Resources, Selector};
        let mut found: Option<&str> = None;
        for rule in self.0.scope.0.iter().filter(|r| r.product == "secrets" || r.product == "*") {
            let Resources::Some(sel) = &rule.resources else { return None };
            for s in sel {
                let Selector::Project { project } = s else { return None };
                match found {
                    Some(p) if p != project => return None,
                    _ => found = Some(project),
                }
            }
        }
        found
    }
}

/// The caller of a request, or of an in-process call.
#[derive(Clone, Debug)]
pub struct Caller {
    /// Stable and safe to log: `operator`, `token:tok_…`, `app:<service>`, `preview:<repo>`.
    pub principal: String,
    pub grants: std::sync::Arc<dyn Grants>,
    /// The client's address, for the audit log.
    pub ip: Option<String>,
    /// A machine roll runs or trusts (apps, previews, checks, sandboxes, syncs, a workload
    /// identity's login): the only callers a sensitive key's value reaches.
    pub runtime: bool,
}

impl Caller {
    pub fn operator() -> Caller {
        Caller { principal: "operator".into(), grants: std::sync::Arc::new(Everything), ip: None, runtime: false }
    }

    /// An in-process consumer (roll apps, previews, sandboxes, tasks): everything the caller
    /// handing out the call may do, named for the audit log.
    pub fn consumer(name: &str) -> Caller {
        Caller { principal: name.into(), grants: std::sync::Arc::new(Everything), ip: None, runtime: true }
    }

    /// This caller, kept out of `except` (a project's protected environments) whatever it may
    /// otherwise do.
    pub fn without(&self, except: Vec<String>) -> Caller {
        Caller { principal: self.principal.clone(), grants: std::sync::Arc::new(Narrowed(self.grants.clone(), Except(except))), ip: self.ip.clone(), runtime: self.runtime }
    }

    pub fn allows(&self, action: Action, at: &Scope<'_>) -> bool {
        self.grants.allows(action, at)
    }

    pub fn need(&self, action: Action, at: &Scope<'_>) -> crate::Result<()> {
        if self.allows(action, at) {
            return Ok(());
        }
        if !self.grants.sees(at.project) {
            return Err(crate::Error::NotFound(format!("no project {}", at.project)));
        }
        let place = match (at.env, at.path) {
            (Some(e), Some(p)) if p != "/" => format!("{e}:{p}"),
            (Some(e), _) => e.to_string(),
            _ => at.project.to_string(),
        };
        Err(crate::Error::Forbidden(format!("this token may not {} in {place}", action.as_str())))
    }

    /// Whether the caller can own personal overrides (people, not tokens or services).
    pub fn personal(&self) -> bool {
        !self.principal.starts_with("token:") && !self.principal.contains(':')
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_scopes_narrow_by_environment_path_and_action() {
        let t = TokenScope { project: "prj_a".into(), environments: Some(vec!["production".into()]), paths: Some(vec!["/api".into()]), actions: vec![Action::Read, Action::Reveal] };
        assert!(t.allows(Action::Reveal, &Scope::at("prj_a", "production", "/api/stripe")));
        assert!(!t.allows(Action::Reveal, &Scope::at("prj_a", "production", "/apix")));
        assert!(!t.allows(Action::Reveal, &Scope::at("prj_a", "staging", "/api")));
        assert!(!t.allows(Action::Write, &Scope::at("prj_a", "production", "/api")));
        assert!(!t.allows(Action::Read, &Scope::at("prj_b", "production", "/api")));
        assert!(t.allows(Action::Read, &Scope::project("prj_a")), "lists the environments");
        assert!(!t.allows(Action::Reveal, &Scope::env("prj_a", "production")), "a reveal names its folder");
    }
}

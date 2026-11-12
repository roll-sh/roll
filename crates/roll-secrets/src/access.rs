//! Who is calling and what they may do (docs/secrets.md, "Access").
//!
//! The roll-wide permission model (docs/permissions.md, branch `feature/roll-mcp`) classifies each
//! request in rolld's gate. Inside the product, the checks that the gate can't make from the
//! request alone (a second environment, a path inside a batch) go through [`Grants`], the one
//! trait to swap when roll-access lands: today it is the operator (everything) or one of the
//! project's scoped tokens (`rlst_…`, [`TokenScope`]).

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

/// What a caller may do. The swap point for roll-access's `Grant`.
pub trait Grants: Send + Sync + std::fmt::Debug {
    fn allows(&self, action: Action, at: &Scope<'_>) -> bool;
    /// The one project the caller is limited to, if any (`whoami`, and the CLI's default).
    fn project(&self) -> Option<&str> {
        None
    }
}

/// The operator: everything.
#[derive(Debug)]
pub struct Everything;

impl Grants for Everything {
    fn allows(&self, _: Action, _: &Scope<'_>) -> bool {
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
}

impl Caller {
    pub fn operator() -> Caller {
        Caller { principal: "operator".into(), grants: std::sync::Arc::new(Everything), ip: None }
    }

    /// An in-process consumer (roll apps, previews, sandboxes, tasks): everything the caller
    /// handing out the call may do, named for the audit log.
    pub fn consumer(name: &str) -> Caller {
        Caller { principal: name.into(), grants: std::sync::Arc::new(Everything), ip: None }
    }

    /// An in-process consumer that may not touch `except` (a project's protected environments).
    pub fn consumer_except(name: &str, except: Vec<String>) -> Caller {
        Caller { principal: name.into(), grants: std::sync::Arc::new(Except(except)), ip: None }
    }

    pub fn allows(&self, action: Action, at: &Scope<'_>) -> bool {
        self.grants.allows(action, at)
    }

    pub fn need(&self, action: Action, at: &Scope<'_>) -> crate::Result<()> {
        if self.allows(action, at) {
            return Ok(());
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

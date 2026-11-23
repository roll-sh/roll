//! Project roles as rules (docs/members.md, "Built-in roles"): a membership is person × project ×
//! role, and a role is a fixed template of [`scope::Rule`]s over that one project. A person's
//! session grant is the union of their memberships' rules ([`session_scope`]), decided by the one
//! evaluator like any token's. Pure: no I/O.
//!
//! Every choice the user may still change (docs/members.md, "Open decisions") is a constant in
//! [`decisions`], so changing one is one edit here.

use crate::scope::{Action, Effect, Resources, Rule, Scope, Selector};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The open decisions D1–D9, each as the design's option (a).
pub mod decisions {
    /// D2: a viewer reveals no values (viewer is roll's `read`: names and history only).
    pub const VIEWER_REVEALS: bool = false;
    /// D3: a developer's default environments, when the membership names none.
    pub const DEVELOPER_DEFAULT: super::DeveloperDefault = super::DeveloperDefault::Unprotected;
    /// D4: a project admin (and owner) reveals in every environment, production too.
    pub const ADMIN_REVEALS_EVERYWHERE: bool = true;
    /// D5: an invite to an address without a roll account waits until roll's operator or an admin
    /// makes the account (roll accounts are invite-only); project admins never make accounts.
    pub const INVITES_MAKE_ACCOUNTS: bool = false;
    /// D6: a resource made through a project has that project as its home, and the home is a
    /// source of trust (roll-workspace's `Workspace::trusts`).
    pub const HOMES: bool = true;
    /// D7: removing a member revokes their tokens confined to the project at once (audited);
    /// tokens reaching other projects only narrow.
    pub const REVOKE_CONFINED_ON_REMOVE: bool = true;
    /// D8: the operator's projects take members (the operator or an admin adds them).
    pub const OPERATOR_PROJECTS_TAKE_MEMBERS: bool = true;
    /// D9: roll's admin accounts stay the operator in every project, whatever their membership.
    pub const ADMIN_IS_OPERATOR: bool = true;
}

/// D3's options: where a developer's role applies in full when the membership names no
/// environments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeveloperDefault {
    /// Every environment the product says isn't protected (`unprotected` rules), never `production`
    /// unless named.
    Unprotected,
    /// Every environment.
    All,
    /// None until named: a viewer everywhere.
    None,
}

/// The products whose routes are open to members (docs/members.md, "Phasing": v1 is workspace and
/// secrets, v1.1 previews and search). Other products' routes keep answering a person as they did.
pub const MEMBER_PRODUCTS: &[&str] = &["workspace", "secrets", "previews", "search"];

/// The built-in roles (D1: `viewer`, `developer`, `admin`, `owner`), weakest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Viewer,
    Developer,
    Admin,
    Owner,
}

impl Role {
    pub const ALL: [Role; 4] = [Role::Viewer, Role::Developer, Role::Admin, Role::Owner];

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Developer => "developer",
            Role::Admin => "admin",
            Role::Owner => "owner",
        }
    }

    pub fn parse(s: &str) -> Option<Role> {
        Role::ALL.into_iter().find(|r| r.as_str() == s)
    }

    /// What the role may do to the project itself (the workspace product): its members are managed
    /// with `admin`, and only an owner deletes it.
    fn workspace(self) -> &'static [Action] {
        match self {
            Role::Viewer | Role::Developer => &[Action::Read],
            Role::Admin => &[Action::Read, Action::Write, Action::Admin],
            Role::Owner => &[Action::Read, Action::Write, Action::Delete, Action::Admin],
        }
    }
}

/// One membership as the rules see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Membership {
    /// `prj_…`
    pub project: String,
    pub role: Role,
    /// A developer's environments, where the role applies in full (`None`: [`decisions::DEVELOPER_DEFAULT`]).
    pub environments: Option<Vec<String>>,
}

fn allow(actions: &[Action]) -> BTreeMap<Action, Effect> {
    actions.iter().map(|a| (*a, Effect::Allow)).collect()
}

fn on(product: &str, project: &str, actions: &[Action]) -> Rule {
    Rule { product: product.into(), resources: Resources::Some(vec![Selector::Project { project: project.into() }]), environments: None, paths: None, unprotected: false, actions: allow(actions) }
}

/// The rules of one membership: the project itself, and each member product in it.
pub fn rules(m: &Membership) -> Vec<Rule> {
    let p = m.project.as_str();
    let mut out = vec![on("workspace", p, m.role.workspace())];
    for product in MEMBER_PRODUCTS.iter().filter(|p| **p != "workspace") {
        let viewer: &[Action] = if decisions::VIEWER_REVEALS { &[Action::Read, Action::Reveal] } else { &[Action::Read] };
        match m.role {
            Role::Viewer => out.push(on(product, p, viewer)),
            Role::Developer => {
                out.push(on(product, p, viewer));
                let full = on(product, p, &[Action::Read, Action::Write, Action::Reveal]);
                match (&m.environments, decisions::DEVELOPER_DEFAULT) {
                    (Some(envs), _) => out.push(Rule { environments: Some(envs.clone()), ..full }),
                    (None, DeveloperDefault::Unprotected) => out.push(Rule { unprotected: true, ..full }),
                    (None, DeveloperDefault::All) => out.push(full),
                    (None, DeveloperDefault::None) => {}
                }
            }
            Role::Admin | Role::Owner => {
                let mut every = Action::ALL.to_vec();
                if !decisions::ADMIN_REVEALS_EVERYWHERE {
                    every.retain(|a| *a != Action::Reveal);
                }
                out.push(on(product, p, &every));
            }
        }
    }
    out
}

/// A person's session grant: every membership's rules.
pub fn session_scope(memberships: &[Membership]) -> Scope {
    Scope(memberships.iter().flat_map(rules).collect())
}

/// Whether `granter` may give (or take away) `role` in `project`: the role's rules are covered by
/// the granter's (`Scope::covers`), so nobody grants more than they hold. An admin can't make, or
/// remove, an owner, because the owner's rules (deleting the project) aren't an admin's.
pub fn may_grant(granter: &Scope, project: &str, role: Role, environments: Option<&[String]>) -> bool {
    granter.covers(&Scope(rules(&Membership { project: project.into(), role, environments: environments.map(<[String]>::to_vec) })))
}

/// Whether `scope` names `project` in any rule: a session that doesn't is a stranger to it, and the
/// project answers 404 as if it didn't exist.
pub fn names_project(scope: &Scope, project: &str) -> bool {
    scope.0.iter().any(|r| match &r.resources {
        Resources::All => true,
        Resources::Some(sel) => sel.iter().any(|s| matches!(s, Selector::Project { project: p } if p == project) || matches!(s, Selector::Id { id, .. } if id == project)),
    })
}

/// Whether rules `scope` stay inside `project`: every rule, of any product, names only this
/// project (`{project}`, or secrets' own `{id}`, which is the project's id), never `*` or another
/// project, and nothing they don't allow asks a person instead (`ask_otherwise`). Read as data, not
/// evaluated: anything it can't prove confined reaches further.
pub fn confined_to(scope: &Scope, ask_otherwise: bool, project: &str) -> bool {
    !ask_otherwise
        && scope.0.iter().all(|r| match &r.resources {
            Resources::All => false,
            Resources::Some(sel) => sel.iter().all(|s| match s {
                Selector::Project { project: p } => p == project,
                Selector::Id { id, .. } => r.product == "secrets" && id == project,
            }),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scope::{Outcome, Resource, Target};

    fn secret(env: &str, protected: bool) -> Target {
        Target::One(Resource { project: Some("prj_a".into()), environment: Some(env.into()), protected: Some(protected), ..Resource::new("secrets", "prj_a") })
    }

    fn scope(role: Role, environments: Option<Vec<String>>) -> Scope {
        session_scope(&[Membership { project: "prj_a".into(), role, environments }])
    }

    /// Each role allows and denies what docs/members.md's role table says, and nobody grants more
    /// than they hold.
    #[test]
    fn roles_are_rules() {
        let (viewer, dev, admin, owner) = (scope(Role::Viewer, None), scope(Role::Developer, None), scope(Role::Admin, None), scope(Role::Owner, None));
        assert_eq!(viewer.outcome(&secret("development", false), Action::Read), Outcome::Allow);
        assert_eq!(viewer.outcome(&secret("development", false), Action::Reveal), Outcome::Deny, "D2: a viewer sees no values");
        assert_eq!(dev.outcome(&secret("development", false), Action::Reveal), Outcome::Allow);
        // What a new project has: development and staging plain, production protected.
        assert_eq!(dev.outcome(&secret("staging", false), Action::Write), Outcome::Allow);
        assert_eq!(dev.outcome(&secret("production", true), Action::Reveal), Outcome::Deny, "D3: not a protected environment");
        assert_eq!(dev.outcome(&secret("production", true), Action::Read), Outcome::Allow);
        // A project made before production started protected: still only when named.
        assert_eq!(dev.outcome(&secret("production", false), Action::Reveal), Outcome::Deny, "D3: production only when named");
        assert_eq!(dev.outcome(&secret("production", false), Action::Write), Outcome::Deny);
        assert_eq!(dev.outcome(&secret("development", false), Action::Delete), Outcome::Deny);
        let named = scope(Role::Developer, Some(vec!["production".into()]));
        assert_eq!(named.outcome(&secret("production", true), Action::Reveal), Outcome::Allow, "production when named");
        assert_eq!(named.outcome(&secret("production", false), Action::Reveal), Outcome::Allow);
        assert_eq!(named.outcome(&secret("development", false), Action::Reveal), Outcome::Deny, "only the named ones");
        assert_eq!(admin.outcome(&secret("production", true), Action::Reveal), Outcome::Allow, "D4");
        assert_eq!(admin.outcome(&secret("production", true), Action::Admin), Outcome::Allow);
        let project = Target::One(Resource { project: Some("prj_a".into()), ..Resource::new("workspace", "prj_a") });
        assert_eq!(admin.outcome(&project, Action::Delete), Outcome::Deny);
        assert_eq!(owner.outcome(&project, Action::Delete), Outcome::Allow);
        assert_eq!(dev.outcome(&Target::One(Resource { project: Some("prj_b".into()), ..Resource::new("secrets", "prj_b") }), Action::Read), Outcome::Deny);
        // Granting: up to your own role, never past it.
        assert!(may_grant(&admin, "prj_a", Role::Admin, None) && may_grant(&admin, "prj_a", Role::Developer, Some(&["production".into()])));
        assert!(!may_grant(&admin, "prj_a", Role::Owner, None), "an admin can't make an owner");
        assert!(may_grant(&owner, "prj_a", Role::Owner, None));
        assert!(!may_grant(&owner, "prj_b", Role::Viewer, None), "nothing in another project");
        assert!(!may_grant(&viewer, "prj_a", Role::Developer, None));
        assert!(names_project(&viewer, "prj_a") && !names_project(&viewer, "prj_b"));
    }
}
